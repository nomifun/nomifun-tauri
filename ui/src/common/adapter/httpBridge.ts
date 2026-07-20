/**
 * HTTP/WS bridge factory — drop-in replacement for bridge.buildProvider / bridge.buildEmitter
 * that routes calls to nomicore via REST API and WebSocket.
 *
 * Exported helpers produce objects with the same shape as @/platform bridge,
 * so existing renderer code works without changes.
 */

// ---------------------------------------------------------------------------
// Base URL
// ---------------------------------------------------------------------------

declare global {
  interface Window {
    __backendPort?: number;
    /**
     * Per-boot local-trust secret injected by the Tauri desktop shell
     * (`apps/desktop/src/main.rs`). The renderer presents it on every request so
     * the desktop's own webview is trusted with no login while remote LAN
     * browsers must authenticate. Absent in WebUI browser mode.
     */
    __nomiLocalTrust?: string;
  }
}

/**
 * Dev-log gating. PTY output and streaming responses arrive as a high-frequency
 * flood of WebSocket messages / HTTP calls; logging each one drowns the console
 * when a `claude` terminal runs. Default OFF. Opt in at runtime with
 * `localStorage.setItem('debug:ws', '1')` (or `'debug:http'`).
 */
const isDebugEnabled = (key: 'debug:ws' | 'debug:http'): boolean => {
  try {
    return typeof localStorage !== 'undefined' && localStorage.getItem(key) === '1';
  } catch {
    return false;
  }
};

/** Event names that fire per PTY chunk / per stream token — never auto-logged. */
const NOISY_WS_EVENTS = new Set(['terminal.output', 'message.stream', 'conversation.artifact']);

/** Path fragments that fire per keystroke / per chunk — never auto-logged. */
const NOISY_HTTP_FRAGMENTS = ['/input', '/resize'];

/** CSRF double-submit cookie + header names (must match the backend constants). */
const CSRF_COOKIE_NAME = 'nomifun-csrf-token';
const CSRF_HEADER_NAME = 'x-csrf-token';

/** Local-trust header the desktop webview presents (must match `nomifun_auth::LOCAL_TRUST_HEADER`). */
const LOCAL_TRUST_HEADER = 'x-nomi-local-trust';

/** Window event emitted when an HTTP API response proves the browser session is expired. */
export const AUTH_EXPIRED_EVENT = 'nomifun:auth-expired';

/**
 * The per-boot local-trust secret injected by the Tauri desktop shell, or null
 * in WebUI browser mode (where auth is via login/JWT cookie instead).
 */
function getLocalTrustSecret(): string | null {
  if (typeof window !== 'undefined' && (window as Window).__nomiLocalTrust) {
    return (window as Window).__nomiLocalTrust as string;
  }
  const g = globalThis as typeof globalThis & { __nomiLocalTrust?: string };
  return g.__nomiLocalTrust ?? null;
}

/** HTTP methods the backend CSRF middleware guards (state-changing). */
const MUTATING_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

/** Read a non-HttpOnly cookie value from `document.cookie`, or null if absent. */
function readCookie(name: string): string | null {
  if (typeof document === 'undefined') return null;
  const prefix = `${name}=`;
  for (const part of document.cookie.split(';')) {
    const trimmed = part.trim();
    if (trimmed.startsWith(prefix)) {
      return decodeURIComponent(trimmed.slice(prefix.length));
    }
  }
  return null;
}

/**
 * Resolve the backend port, honoring both renderer and main-process contexts.
 *
 * - Renderer (Electron): the preload bridge writes `window.__backendPort` before
 *   the first HTTP call, so reading from window is authoritative.
 * - Renderer (WebUI browser): no preload, so `window.__backendPort` is missing.
 *   Requests must go to the same origin that served the page; web-host's
 *   static-server reverse-proxies `/api/*` and upgrades `/ws` to the backend
 *   port. See getBaseUrl / getWsUrl below for the WebUI branch.
 * - Main process: `window` is undefined. `src/index.ts` writes the port to
 *   `globalThis.__backendPort` immediately after `backendManager.start()`
 *   resolves, so any main-process ipcBridge caller hits the correct port.
 * - Fallback `13400` only applies when neither is initialized — the request
 *   will still fail cleanly with ECONNREFUSED rather than masking the bug.
 */
function getBackendPort(): number {
  if (typeof window !== 'undefined' && (window as Window).__backendPort) {
    return (window as Window).__backendPort as number;
  }
  const g = globalThis as typeof globalThis & { __backendPort?: number };
  return g.__backendPort ?? 13400;
}

/**
 * WebUI (browser) mode: no Electron preload, so `window.__backendPort` is not
 * injected. Use same-origin URLs; web-host's static-server handles the reverse
 * proxy / WS upgrade to the backend.
 */
function isWebUiBrowserMode(): boolean {
  return typeof window !== 'undefined' && typeof document !== 'undefined' && !(window as Window).__backendPort;
}

/**
 * Build the auth/CSRF headers every backend request must carry.
 *
 * Single source of truth shared by `httpRequest` (fetch/JSON) and the multipart
 * upload `XMLHttpRequest` in `FileService`. The desktop shell's `fetch`
 * interceptor (`apps/desktop/src/main.rs`) only patches `window.fetch`, so a raw
 * XHR escapes it — without applying these headers itself the upload reaches the
 * `TrustLocalToken`-guarded `/api/fs/upload` with no `x-nomi-local-trust` and is
 * rejected 403. In WebUI browser mode the same XHR also needs the CSRF header on
 * state-changing requests.
 *
 * @param method HTTP method — decides whether the CSRF (mutating) header applies.
 */
export function buildBackendAuthHeaders(method: string): Record<string, string> {
  const headers: Record<string, string> = {};

  // Desktop shell: present the per-boot local-trust secret so the backend
  // (running under TrustLocalToken) recognizes this webview as the trusted
  // local client and skips login. Absent in WebUI browser mode.
  const trustSecret = getLocalTrustSecret();
  if (trustSecret) {
    headers[LOCAL_TRUST_HEADER] = trustSecret;
  }

  // In WebUI browser mode the backend runs authenticated, which enables the
  // CSRF double-submit guard. Echo the (non-HttpOnly) csrf cookie into the
  // x-csrf-token header on state-changing requests. In desktop (Tauri) mode the
  // backend runs local/no-CSRF and the cookie is absent, so this is a no-op.
  if (isWebUiBrowserMode() && MUTATING_METHODS.has(method.toUpperCase())) {
    const csrf = readCookie(CSRF_COOKIE_NAME);
    if (csrf) {
      headers[CSRF_HEADER_NAME] = csrf;
    }
  }

  return headers;
}

export function getBaseUrl(): string {
  if (isWebUiBrowserMode()) {
    // Same-origin: calls like fetch(`${baseUrl}/api/foo`) resolve to `/api/foo`
    // on whatever host the page was served from.
    return '';
  }
  return `http://127.0.0.1:${getBackendPort()}`;
}

function getWsUrl(): string {
  if (isWebUiBrowserMode()) {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${proto}//${window.location.host}/ws`;
  }
  return `ws://127.0.0.1:${getBackendPort()}/ws`;
}

// ---------------------------------------------------------------------------
// Structured backend error
// ---------------------------------------------------------------------------

/**
 * Error thrown by `httpRequest` when the backend returns a non-2xx response.
 * Carries the structured error envelope (`success: false, error, code`) so
 * callers can branch on `code` without parsing the stringified message.
 *
 * @example
 *   try { await ipcBridge.conversation.sendMessage.invoke(...); }
 *   catch (e) {
 *     if (isBackendHttpError(e) && e.code === 'CONVERSATION_ARCHIVED') { ... }
 *   }
 */
export class BackendHttpError extends Error {
  readonly status: number;
  /** Machine-readable error code from the backend `ErrorResponse.code`, or `''` when parse failed. */
  readonly code: string;
  /** Backend-provided human message from `ErrorResponse.error`, or the raw body when parse failed. */
  readonly backendMessage: string;
  /** True when the backend rejected a browser session token as missing/expired/invalid. */
  readonly authExpired: boolean;
  /** True when the WebUI login redirect/event handler was eligible to handle this error. */
  readonly authExpiredHandled: boolean;
  /** Structured backend metadata from `ErrorResponse.details`, when present. */
  readonly details: unknown;
  /** Raw parsed body (object on JSON response, string on text/non-JSON). */
  readonly body: unknown;

  constructor(params: { method: string; path: string; status: number; body: unknown }) {
    const { method, path, status, body } = params;
    let code = '';
    let backendMessage = '';
    let details: unknown;
    if (body && typeof body === 'object') {
      const b = body as { code?: unknown; error?: unknown; details?: unknown };
      if (typeof b.code === 'string') code = b.code;
      if (typeof b.error === 'string') backendMessage = b.error;
      details = b.details;
    } else if (typeof body === 'string') {
      backendMessage = body;
    }
    const authExpired = isAuthExpiredResponse(status, body);
    const authExpiredHandled = authExpired && isWebUiBrowserMode();
    super(
      authExpired
        ? `Backend ${method} ${path} failed (${status}): authentication expired`
        : `Backend ${method} ${path} failed (${status}): ${JSON.stringify(body)}`
    );
    this.name = 'BackendHttpError';
    this.status = status;
    this.code = code;
    this.backendMessage = backendMessage;
    this.authExpired = authExpired;
    this.authExpiredHandled = authExpiredHandled;
    this.details = details;
    this.body = body;
  }
}

export function isBackendHttpError(error: unknown): error is BackendHttpError {
  // Prefer instanceof — fast path in production/bundled contexts.
  if (error instanceof BackendHttpError) return true;
  // Fallback: vite-dev HMR can split the module across chunks, breaking
  // instanceof. Detect by duck-typing on the shape produced by our
  // constructor.
  if (
    error &&
    typeof error === 'object' &&
    'name' in error &&
    (error as { name: unknown }).name === 'BackendHttpError' &&
    'status' in error &&
    typeof (error as { status: unknown }).status === 'number' &&
    'code' in error &&
    typeof (error as { code: unknown }).code === 'string'
  ) {
    return true;
  }
  return false;
}

function bodyStringField(body: unknown, key: 'code' | 'error'): string {
  if (!body || typeof body !== 'object') return '';
  const value = (body as Record<string, unknown>)[key];
  return typeof value === 'string' ? value : '';
}

function isAuthExpiredResponse(status: number, body: unknown): boolean {
  if (status !== 403) return false;
  const code = bodyStringField(body, 'code');
  const error = bodyStringField(body, 'error').toLowerCase();
  if (code !== 'FORBIDDEN') return false;
  return (
    error.includes('invalid or expired token') ||
    error.includes('authentication required') ||
    error.includes('user not found')
  );
}

export function isAuthExpiredHttpError(error: unknown): boolean {
  return isBackendHttpError(error) && (error.authExpired === true || isAuthExpiredResponse(error.status, error.body));
}

export function isHandledAuthExpiredHttpError(error: unknown): boolean {
  return isBackendHttpError(error) && error.authExpiredHandled === true;
}

function clearBrowserAuthArtifacts(): void {
  try {
    if (typeof localStorage !== 'undefined') {
      const keysToRemove: string[] = [];
      for (let i = 0; i < localStorage.length; i += 1) {
        const key = localStorage.key(i);
        if (key && /auth|csrf|token/i.test(key)) {
          keysToRemove.push(key);
        }
      }
      keysToRemove.forEach((key) => localStorage.removeItem(key));
    }
  } catch {
    // Best-effort cleanup only.
  }

  try {
    if (typeof document !== 'undefined') {
      document.cookie = `${CSRF_COOKIE_NAME}=; Path=/; Max-Age=0`;
      document.cookie = `csrf-token=; Path=/; Max-Age=0`;
    }
  } catch {
    // Best-effort cleanup only.
  }
}

function emitAuthExpiredEvent(): void {
  if (typeof window === 'undefined' || typeof window.dispatchEvent !== 'function') return;
  const event =
    typeof CustomEvent === 'function'
      ? new CustomEvent(AUTH_EXPIRED_EVENT)
      : typeof Event === 'function'
        ? new Event(AUTH_EXPIRED_EVENT)
        : ({ type: AUTH_EXPIRED_EVENT } as Event);
  window.dispatchEvent(event);
}

function handleHttpAuthExpired(): void {
  // Desktop shell requests should be trusted by x-nomi-local-trust, not redirected
  // into the WebUI login flow. If a desktop request reaches this branch, keep the
  // original backend error visible so the trust-header path can be diagnosed.
  if (!isWebUiBrowserMode()) return;

  clearBrowserAuthArtifacts();
  emitAuthExpiredEvent();

  setTimeout(() => {
    try {
      if (window.location.pathname === '/login' || window.location.hash.includes('/login')) return;
      window.location.hash = '/login';
    } catch {
      // Nothing else to do; the thrown BackendHttpError still reaches callers.
    }
  }, 0);
}

/**
 * Error thrown by `httpRequest` when the request never produced an HTTP
 * response — i.e. a transport-layer failure, not a non-2xx status
 * ([`BackendHttpError`] covers that). Two shapes:
 *
 * - `kind: 'timeout'` — the optional per-request `timeoutMs` deadline elapsed
 *   and we aborted the request (the backend was unreachable or too slow, e.g.
 *   a knowledge-base root on an offline/slow network drive).
 * - `kind: 'network'` — `fetch` itself rejected. In the desktop WKWebView this
 *   surfaces as the opaque `TypeError: Load failed`; we wrap it with a
 *   diagnosable message instead of letting that raw string escape to the UI.
 */
export class BackendRequestError extends Error {
  readonly kind: 'timeout' | 'network';
  constructor(kind: 'timeout' | 'network', message: string) {
    super(message);
    this.name = 'BackendRequestError';
    this.kind = kind;
  }
}

export function isBackendRequestError(error: unknown): error is BackendRequestError {
  return (
    error instanceof BackendRequestError ||
    (!!error &&
      typeof error === 'object' &&
      'name' in error &&
      (error as { name: unknown }).name === 'BackendRequestError')
  );
}

// ---------------------------------------------------------------------------
// HTTP request helper
// ---------------------------------------------------------------------------

/**
 * Per-request overrides for `httpRequest`.
 *
 * `silentStatuses` lets known-soft failures (e.g. `GET /:id/model` returning
 * 404 before the agent has attached) skip the noisy `console.error` and the
 * Sentry breadcrumb that comes with it. The error is still thrown so the
 * caller's existing try/catch keeps working.
 */
export type HttpRequestOptions = {
  silentStatuses?: number[];
  /**
   * Optional client-side deadline in milliseconds. When set, the request is
   * aborted after this long and a legible [`BackendRequestError`] (`timeout`)
   * is thrown instead of hanging until the platform's own network timeout.
   *
   * Apply only where the caller can safely recover from a client-side abort.
   * A mutation may continue on the server after the browser stops waiting, so
   * timed mutations must be idempotent or serialized by the backend.
   */
  timeoutMs?: number;
};

const SENSITIVE_LOG_KEY_PATTERN = /api[_-]?key|authorization|auth[_-]?token|access[_-]?token|refresh[_-]?token|secret/i;

function redactForLog(value: unknown, depth = 0): unknown {
  if (depth > 8 || value === null || typeof value !== 'object') {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map((item) => redactForLog(item, depth + 1));
  }

  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>).map(([key, entry]) => [
      key,
      SENSITIVE_LOG_KEY_PATTERN.test(key) ? '[REDACTED]' : redactForLog(entry, depth + 1),
    ])
  );
}

export async function httpRequest<T>(
  method: string,
  path: string,
  body?: unknown,
  options?: HttpRequestOptions
): Promise<T> {
  const url = `${getBaseUrl()}${path}`;
  const headers: Record<string, string> = {};

  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
  }

  // Trust (desktop) + CSRF (WebUI) headers — shared with the FileService upload XHR.
  Object.assign(headers, buildBackendAuthHeaders(method));

  const isNoisyPath = NOISY_HTTP_FRAGMENTS.some((frag) => path.includes(frag));
  if (isDebugEnabled('debug:http') && !isNoisyPath) {
    console.debug(
      `[httpBridge] ${method} ${path}`,
      body !== undefined ? JSON.stringify(redactForLog(body)).slice(0, 500) : '(no body)'
    );
  }

  // Optional client-side deadline (opt-in via options.timeoutMs). Aborts a
  // request that outlives it so a hung/unreachable backend surfaces a legible
  // error instead of the opaque platform network timeout minutes later.
  const controller = options?.timeoutMs != null ? new AbortController() : undefined;
  const timeoutHandle =
    controller && options?.timeoutMs != null
      ? setTimeout(() => controller.abort(), options.timeoutMs)
      : undefined;

  let response: Response;
  try {
    response = await fetch(url, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: controller?.signal,
      // Every GET issued through this bridge targets mutable application state
      // (conversation history, settings, catalogs, runtime status, ...).  The
      // same URL is intentionally reused when a view remounts, so allowing the
      // host WebView's HTTP cache to satisfy it can resurrect a pre-mutation
      // response.  WebKitGTK is particularly eager to reuse loopback GETs, but
      // the contract must be identical on macOS WKWebView, Windows WebView2 and
      // ordinary WebUI browsers: a bridge read always observes the backend.
      cache: method.toUpperCase() === 'GET' ? 'no-store' : undefined,
    });
  } catch (e) {
    // No HTTP response was produced: our own timeout abort, or a transport
    // failure (backend unreachable / connection reset). WKWebView renders the
    // latter as an opaque "TypeError: Load failed"; rethrow something the UI
    // and logs can actually act on.
    if (controller?.signal.aborted) {
      throw new BackendRequestError(
        'timeout',
        `Backend ${method} ${path} timed out after ${options?.timeoutMs}ms; the backend may be busy or unreachable`
      );
    }
    const detail = e instanceof Error ? e.message : String(e);
    throw new BackendRequestError('network', `Backend ${method} ${path} failed: backend unreachable (${detail})`);
  } finally {
    if (timeoutHandle !== undefined) clearTimeout(timeoutHandle);
  }

  if (!response.ok) {
    // Read the body exactly once. A `Response` body is a one-shot stream, so
    // calling `.json()` then `.text()` (e.g. as a parse fallback) throws
    // "body stream already read". Many error responses have an empty or
    // non-JSON body (axum-default 404/405, plain-text 5xx), so read text first
    // and opportunistically parse it as JSON.
    const rawText = await response.text();
    let errorBody: unknown;
    try {
      errorBody = rawText ? JSON.parse(rawText) : '';
    } catch {
      errorBody = rawText;
    }
    const authExpired = isAuthExpiredResponse(response.status, errorBody);
    if (authExpired) {
      handleHttpAuthExpired();
    }
    if (authExpired) {
      if (isDebugEnabled('debug:http') && !isNoisyPath) {
        console.debug(`[httpBridge] ${method} ${path} → ${response.status} (auth-expired)`, errorBody);
      }
    } else if (options?.silentStatuses?.includes(response.status)) {
      console.debug(`[httpBridge] ${method} ${path} → ${response.status} (silenced)`, errorBody);
    } else {
      console.error(`[httpBridge] ${method} ${path} → ${response.status}`, errorBody);
    }
    throw new BackendHttpError({ method, path, status: response.status, body: errorBody });
  }

  if (isDebugEnabled('debug:http') && !isNoisyPath) {
    console.debug(`[httpBridge] ${method} ${path} → ${response.status} OK`);
  }

  const contentType = response.headers.get('Content-Type');
  if (!contentType?.includes('application/json')) {
    return undefined as T;
  }

  const json = await response.json();
  // Backend wraps in { success, data, ... } — unwrap when present
  if (json && typeof json === 'object' && 'data' in json) {
    return json.data as T;
  }
  return json as T;
}

// ---------------------------------------------------------------------------
// Provider factories (same shape as bridge.buildProvider)
// ---------------------------------------------------------------------------

type ProviderLike<Data, Params> = {
  provider: (handler: (params: Params) => Promise<Data>) => void;
  invoke: Params extends undefined ? () => Promise<Data> : (params: Params) => Promise<Data>;
};

type ResponseMapInvoker<Data, Args extends unknown[]> = {
  provider: (...args: never[]) => void;
  invoke: (...args: Args) => Promise<Data>;
};

export function withResponseMap<Raw, Mapped, Args extends unknown[]>(
  inner: ResponseMapInvoker<Raw, Args>,
  map: (data: Raw) => Mapped
): ResponseMapInvoker<Mapped, Args> {
  return {
    provider: () => {},
    invoke: async (...args: Args) => {
      const raw = await inner.invoke(...args);
      return map(raw);
    },
  };
}

export function httpGet<Data, Params = undefined>(
  path: string | ((params: Params) => string),
  options?: HttpRequestOptions
): ProviderLike<Data, Params> {
  return {
    provider: () => {},
    invoke: (async (params?: Params) => {
      const resolvedPath = typeof path === 'function' ? path(params!) : path;
      return httpRequest<Data>('GET', resolvedPath, undefined, options);
    }) as ProviderLike<Data, Params>['invoke'],
  };
}

export function httpPost<Data, Params = undefined>(
  path: string | ((params: Params) => string),
  mapBody?: (params: Params) => unknown,
  options?: HttpRequestOptions
): ProviderLike<Data, Params> {
  return {
    provider: () => {},
    invoke: (async (params?: Params) => {
      const resolvedPath = typeof path === 'function' ? path(params!) : path;
      const body = mapBody ? mapBody(params!) : params;
      return httpRequest<Data>('POST', resolvedPath, body, options);
    }) as ProviderLike<Data, Params>['invoke'],
  };
}

export function httpPut<Data, Params = undefined>(
  path: string | ((params: Params) => string),
  mapBody?: (params: Params) => unknown
): ProviderLike<Data, Params> {
  return {
    provider: () => {},
    invoke: (async (params?: Params) => {
      const resolvedPath = typeof path === 'function' ? path(params!) : path;
      const body = mapBody ? mapBody(params!) : params;
      return httpRequest<Data>('PUT', resolvedPath, body);
    }) as ProviderLike<Data, Params>['invoke'],
  };
}

export function httpPatch<Data, Params = undefined>(
  path: string | ((params: Params) => string),
  mapBody?: (params: Params) => unknown
): ProviderLike<Data, Params> {
  return {
    provider: () => {},
    invoke: (async (params?: Params) => {
      const resolvedPath = typeof path === 'function' ? path(params!) : path;
      const body = mapBody ? mapBody(params!) : params;
      return httpRequest<Data>('PATCH', resolvedPath, body);
    }) as ProviderLike<Data, Params>['invoke'],
  };
}

export function httpDelete<Data, Params = undefined>(
  path: string | ((params: Params) => string)
): ProviderLike<Data, Params> {
  return {
    provider: () => {},
    invoke: (async (params?: Params) => {
      const resolvedPath = typeof path === 'function' ? path(params!) : path;
      return httpRequest<Data>('DELETE', resolvedPath);
    }) as ProviderLike<Data, Params>['invoke'],
  };
}

/**
 * Stub provider for features not yet implemented in the backend.
 * Returns a sensible default value and logs a warning.
 */
export function stubProvider<Data, Params = undefined>(name: string, defaultValue: Data): ProviderLike<Data, Params> {
  return {
    provider: () => {},
    invoke: (async (_params?: Params) => {
      console.warn(`[httpBridge] stub: ${name} not yet implemented in backend`);
      return defaultValue;
    }) as ProviderLike<Data, Params>['invoke'],
  };
}

// ---------------------------------------------------------------------------
// WebSocket singleton
// ---------------------------------------------------------------------------

type WsCallback = (data: unknown) => void;
const wsListeners = new Map<string, Set<WsCallback>>();
let ws: WebSocket | null = null;
let wsReconnectTimer: ReturnType<typeof setTimeout> | null = null;
let wsReconnectAttempt = 0;

function ensureWs(): void {
  if (typeof window === 'undefined') {
    console.debug('[ensureWs] skipped: no window');
    return;
  }
  if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
    console.debug('[ensureWs] skipped: already open/connecting, readyState=', ws.readyState);
    return;
  }

  const url = getWsUrl();
  console.debug('[ensureWs] connecting to', url);
  try {
    // Desktop shell: carry the local-trust secret as a WebSocket subprotocol
    // (browsers cannot set custom headers on the WS handshake). The backend
    // reads it from `Sec-WebSocket-Protocol` and echoes it back so the
    // handshake succeeds. WebUI browser mode authenticates via the session
    // cookie instead, so no subprotocol is sent.
    const trustSecret = getLocalTrustSecret();
    ws = trustSecret ? new WebSocket(url, [trustSecret]) : new WebSocket(url);
  } catch (e) {
    console.error('[ensureWs] WebSocket constructor threw:', e);
    scheduleWsReconnect();
    return;
  }

  const current = ws;

  current.addEventListener('open', () => {
    console.debug('[ensureWs] CONNECTED');
    // A non-zero attempt counter means we got here by reconnecting (the socket
    // had dropped and `scheduleWsReconnect` ran). Notify local listeners so a
    // live view can resync: the server only does a live fan-out with no replay,
    // so every frame emitted while the socket was down was lost. Dispatch BEFORE
    // resetting the counter. `ws.reconnected` is a synthetic local event name,
    // never sent by the server.
    const wasReconnect = wsReconnectAttempt > 0;
    wsReconnectAttempt = 0;
    if (wasReconnect) {
      const handlers = wsListeners.get('ws.reconnected');
      if (handlers) {
        for (const h of [...handlers]) {
          try {
            h(undefined);
          } catch {
            /* never crash listener */
          }
        }
      }
    }
  });

  current.addEventListener('close', (e) => {
    console.debug('[ensureWs] CLOSED code=' + e.code + ' reason=' + e.reason);
    if (ws === current) ws = null;
    scheduleWsReconnect();
  });

  current.addEventListener('error', (e) => {
    console.error('[ensureWs] ERROR', e);
    current.close();
  });

  current.addEventListener('message', (event: MessageEvent) => {
    try {
      const msg = JSON.parse(event.data as string) as {
        name?: string;
        event?: string;
        data?: unknown;
        payload?: unknown;
      };
      const eventName = msg.name ?? msg.event;
      const payload = msg.data ?? msg.payload;
      if (eventName === 'ping') {
        if (current.readyState === WebSocket.OPEN) {
          current.send(
            JSON.stringify({
              name: 'pong',
              data: { timestamp: Date.now() },
            })
          );
        }
        return;
      }
      if (isDebugEnabled('debug:ws') && eventName && !NOISY_WS_EVENTS.has(eventName)) {
        console.debug('[WS:msg]', eventName, JSON.stringify(payload).slice(0, 200));
      }
      if (eventName) {
        const handlers = wsListeners.get(eventName);
        if (handlers) {
          for (const h of handlers) {
            try {
              h(payload);
            } catch {
              /* never crash listener */
            }
          }
        }
      }
    } catch {
      // ignore non-JSON
    }
  });
}

function scheduleWsReconnect(): void {
  if (wsReconnectTimer || wsListeners.size === 0) return;
  const delay = Math.min(1000 * Math.pow(2, wsReconnectAttempt), 30000);
  wsReconnectAttempt++;
  wsReconnectTimer = setTimeout(() => {
    wsReconnectTimer = null;
    if (wsListeners.size === 0) return;
    ensureWs();
  }, delay);
}

// ---------------------------------------------------------------------------
// Emitter factory (same shape as bridge.buildEmitter)
// ---------------------------------------------------------------------------

type EmitterLike<Params> = {
  on: (callback: Params extends undefined ? () => void : (params: Params) => void) => () => void;
  emit: Params extends undefined ? () => void : (params: Params) => void;
};

export function wsEmitter<Params = undefined>(eventName: string): EmitterLike<Params> {
  return {
    on: (callback: (params: Params) => void) => {
      if (!wsListeners.has(eventName)) {
        wsListeners.set(eventName, new Set());
      }
      const cb = callback as WsCallback;
      wsListeners.get(eventName)!.add(cb);
      ensureWs();
      return () => {
        const listeners = wsListeners.get(eventName);
        listeners?.delete(cb);
        if (listeners?.size === 0) {
          wsListeners.delete(eventName);
        }
        if (wsListeners.size === 0) {
          if (wsReconnectTimer) {
            clearTimeout(wsReconnectTimer);
            wsReconnectTimer = null;
          }
          wsReconnectAttempt = 0;
        }
      };
    },
    emit: (() => {}) as EmitterLike<Params>['emit'],
  };
}

export function wsMappedEmitter<Params = undefined, Raw = Params>(
  eventName: string,
  transform: (raw: Raw) => Params
): EmitterLike<Params> {
  const inner = wsEmitter<Raw>(eventName);
  return {
    on: (callback: (params: Params) => void) => {
      return (inner.on as (callback: (raw: Raw) => void) => () => void)((raw) => {
        callback(transform(raw));
      });
    },
    emit: (() => {}) as EmitterLike<Params>['emit'],
  };
}

/**
 * Stub emitter for events not yet implemented in the backend.
 */
export function stubEmitter<Params = undefined>(_name: string): EmitterLike<Params> {
  return {
    on: () => () => {},
    emit: (() => {}) as EmitterLike<Params>['emit'],
  };
}
