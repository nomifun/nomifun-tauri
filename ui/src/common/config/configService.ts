import type { ConfigKey, ConfigKeyMap } from './configKeys';

type Subscriber = (value: unknown) => void;

declare global {
  interface Window {
    __backendPort?: number;
    __nomiLocalTrust?: string;
  }
}

function getBaseUrl(): string {
  // WebUI browser mode: no preload, fetch same-origin so web-host's
  // static-server reverse-proxies /api/* to the backend.
  if (typeof window !== 'undefined' && typeof document !== 'undefined' && !(window as Window).__backendPort) {
    return '';
  }
  const port = typeof window !== 'undefined' ? (window as Window).__backendPort || 13400 : 13400;
  return `http://127.0.0.1:${port}`;
}

/** Read the CSRF double-submit cookie (browser mode) for state-changing requests. */
function readCsrfCookie(): string | null {
  if (typeof document === 'undefined') return null;
  for (const part of document.cookie.split(';')) {
    const trimmed = part.trim();
    if (trimmed.startsWith('nomifun-csrf-token=')) {
      return decodeURIComponent(trimmed.slice('nomifun-csrf-token='.length));
    }
  }
  return null;
}

async function fetchJson<T>(method: string, path: string, body?: unknown): Promise<T> {
  const url = `${getBaseUrl()}${path}`;
  const headers: Record<string, string> = {};
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
  }
  // Desktop shell: present the per-boot local-trust secret so the backend
  // (running under TrustLocalToken) recognizes this webview as the trusted
  // local client. Without it, mutating requests (PUT) are CSRF-rejected (403).
  // Must match `httpBridge.ts` since configService bypasses that chokepoint.
  const trustSecret = typeof window !== 'undefined' ? (window as Window).__nomiLocalTrust : undefined;
  if (trustSecret) {
    headers['x-nomi-local-trust'] = trustSecret;
  } else if (
    typeof document !== 'undefined' &&
    ['POST', 'PUT', 'PATCH', 'DELETE'].includes(method.toUpperCase())
  ) {
    // WebUI browser mode (no trust secret): echo the CSRF cookie.
    const csrf = readCsrfCookie();
    if (csrf) headers['x-csrf-token'] = csrf;
  }
  const response = await fetch(url, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    cache: method.toUpperCase() === 'GET' ? 'no-store' : undefined,
  });
  if (!response.ok) {
    const errorBody = await response.text();
    throw new Error(`ConfigService ${method} ${path} failed (${response.status}): ${errorBody}`);
  }
  const contentType = response.headers.get('Content-Type');
  if (!contentType?.includes('application/json')) {
    return undefined as T;
  }
  const json = await response.json();
  if (json && typeof json === 'object' && 'data' in json) {
    return json.data as T;
  }
  return json as T;
}

class ConfigServiceImpl {
  private cache = new Map<string, unknown>();
  private subscribers = new Map<string, Set<Subscriber>>();
  private initialized = false;
  private initPromise: Promise<void> | null = null;

  // Idempotent: concurrent callers share the same in-flight promise, and a
  // resolved init returns immediately. Modules that need persisted settings on
  // module load (theme/colorScheme/language) await whenReady() before reading.
  //
  // IMPORTANT: this NEVER rejects. Before login (WebUI remote browser) the
  // backend returns 401/403 for /api/settings/client, and the network may be
  // unreachable. Those are expected pre-auth states, not fatal errors — the app
  // must still render (the login page!) with empty/default config. On failure we
  // resolve with an empty cache and leave `initialized = false` + clear the
  // in-flight promise so a later call (e.g. right after login via `reload()`)
  // re-fetches the authenticated settings.
  initialize(): Promise<void> {
    if (this.initPromise) return this.initPromise;
    this.initPromise = (async () => {
      try {
        const data = await fetchJson<Record<string, unknown>>('GET', '/api/settings/client');
        this.cache.clear();
        if (data) {
          for (const [key, value] of Object.entries(data)) {
            this.cache.set(key, value);
          }
        }
        this.initialized = true;
      } catch (error) {
        console.warn('[configService] settings unavailable (pre-login or offline); using empty config:', error);
        this.cache.clear();
        this.initPromise = null;
      }
    })();
    return this.initPromise;
  }

  /** Force a re-fetch of settings — call right after login, when the backend
   *  starts returning the authenticated client settings. */
  async reload(): Promise<void> {
    this.initPromise = null;
    this.initialized = false;
    await this.initialize();
  }

  whenReady(): Promise<void> {
    return this.initialize();
  }

  get<K extends ConfigKey>(key: K): ConfigKeyMap[K] | undefined {
    return this.cache.get(key) as ConfigKeyMap[K] | undefined;
  }

  async set<K extends ConfigKey>(key: K, value: ConfigKeyMap[K]): Promise<void> {
    this.cache.set(key, value);
    this.notify(key, value);
    await fetchJson<void>('PUT', '/api/settings/client', { [key]: value });
  }

  setLocal<K extends ConfigKey>(key: K, value: ConfigKeyMap[K]): void {
    this.cache.set(key, value);
    this.notify(key, value);
  }

  async remove(key: ConfigKey): Promise<void> {
    this.cache.delete(key);
    this.notify(key, undefined);
    await fetchJson<void>('PUT', '/api/settings/client', { [key]: null });
  }

  async setBatch(entries: Partial<{ [K in ConfigKey]: ConfigKeyMap[K] }>): Promise<void> {
    for (const [key, value] of Object.entries(entries)) {
      this.cache.set(key, value);
      this.notify(key as ConfigKey, value);
    }
    await fetchJson<void>('PUT', '/api/settings/client', entries);
  }

  subscribe(key: ConfigKey, callback: Subscriber): () => void {
    if (!this.subscribers.has(key)) {
      this.subscribers.set(key, new Set());
    }
    this.subscribers.get(key)!.add(callback);
    return () => {
      this.subscribers.get(key)?.delete(callback);
    };
  }

  isInitialized(): boolean {
    return this.initialized;
  }

  reset(): void {
    this.cache.clear();
    this.subscribers.clear();
    this.initialized = false;
    this.initPromise = null;
  }

  private notify(key: ConfigKey, value: unknown): void {
    const subs = this.subscribers.get(key);
    if (subs) {
      for (const cb of subs) {
        cb(value);
      }
    }
  }
}

export const configService = new ConfigServiceImpl();
