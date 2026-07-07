/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  AUTH_EXPIRED_EVENT,
  httpRequest,
  isAuthExpiredHttpError,
  isBackendHttpError,
  isHandledAuthExpiredHttpError,
} from './httpBridge';

const realFetch = globalThis.fetch;

const realWindow = (globalThis as { window?: Window }).window;
const realDocument = (globalThis as { document?: Document }).document;

function installBrowserGlobals(windowPatch: Partial<Window> & { __backendPort?: number; __nomiLocalTrust?: string }) {
  (globalThis as { window?: unknown }).window = {
    location: { pathname: '/requirements/extensions', hash: '' },
    dispatchEvent: () => true,
    ...windowPatch,
  };
  (globalThis as { document?: unknown }).document = { cookie: '' };
}

function restoreBrowserGlobals() {
  if (realWindow === undefined) {
    delete (globalThis as { window?: Window }).window;
  } else {
    (globalThis as { window?: Window }).window = realWindow;
  }
  if (realDocument === undefined) {
    delete (globalThis as { document?: Document }).document;
  } else {
    (globalThis as { document?: Document }).document = realDocument;
  }
}

describe('httpRequest client deadline + network-failure diagnosis', () => {
  test('aborts and throws a legible timeout error when the request exceeds timeoutMs', async () => {
    // A fetch that never resolves on its own but honors the abort signal —
    // models a backend hung inside a slow/stale NAS walk.
    globalThis.fetch = ((_url: string, init?: RequestInit) =>
      new Promise((_resolve, reject) => {
        init?.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')));
      })) as unknown as typeof fetch;
    try {
      let message = '';
      let isHttp = true;
      try {
        await httpRequest('GET', '/api/knowledge/bases', undefined, { timeoutMs: 10 });
      } catch (e) {
        message = e instanceof Error ? e.message : String(e);
        isHttp = isBackendHttpError(e);
      }
      expect(message.toLowerCase().includes('timed out')).toBe(true);
      // A client-side timeout is NOT an HTTP status error.
      expect(isHttp).toBe(false);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('wraps an opaque network failure (WKWebView "TypeError: Load failed") in a diagnosable error', async () => {
    globalThis.fetch = (() => Promise.reject(new TypeError('Load failed'))) as unknown as typeof fetch;
    try {
      let message = '';
      try {
        await httpRequest('GET', '/api/knowledge/bases');
      } catch (e) {
        message = e instanceof Error ? e.message : String(e);
      }
      expect(message === 'Load failed').toBe(false);
      expect(message.toLowerCase().includes('unreachable')).toBe(true);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('a normal 2xx JSON response is still unwrapped from the { data } envelope', async () => {
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(JSON.stringify({ success: true, data: [{ id: 'kb_1' }] }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      )) as unknown as typeof fetch;
    try {
      const res = await httpRequest<Array<{ id: string }>>('GET', '/api/knowledge/bases', undefined, { timeoutMs: 30000 });
      expect(JSON.stringify(res)).toBe(JSON.stringify([{ id: 'kb_1' }]));
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('webui invalid-session HTTP failures trigger auth-expired handling', async () => {
    const emitted: string[] = [];
    const location = { pathname: '/requirements/extensions', hash: '' };
    installBrowserGlobals({
      location: location as Location,
      dispatchEvent: ((event: Event) => {
        emitted.push(event.type);
        return true;
      }) as Window['dispatchEvent'],
    });
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(JSON.stringify({ success: false, error: 'Forbidden: Invalid or expired token', code: 'FORBIDDEN' }), {
          status: 403,
          headers: { 'Content-Type': 'application/json' },
        })
      )) as unknown as typeof fetch;

    try {
      let caught: unknown;
      try {
        await httpRequest('GET', '/api/requirements/tags');
      } catch (e) {
        caught = e;
      }

      expect(isAuthExpiredHttpError(caught)).toBe(true);
      expect(isHandledAuthExpiredHttpError(caught)).toBe(true);
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(location.hash).toBe('/login');
      expect(emitted.includes(AUTH_EXPIRED_EVENT)).toBe(true);
    } finally {
      globalThis.fetch = realFetch;
      restoreBrowserGlobals();
    }
  });

  test('desktop local-trust requests do not redirect to login when a mocked 403 is returned', async () => {
    const location = { pathname: '/settings/model', hash: '' };
    let capturedHeaders: Record<string, string> | undefined;
    installBrowserGlobals({
      __backendPort: 25808,
      __nomiLocalTrust: 'local-secret',
      location: location as Location,
    });
    globalThis.fetch = ((_url: string, init?: RequestInit) => {
      capturedHeaders = init?.headers as Record<string, string>;
      return Promise.resolve(
        new Response(JSON.stringify({ success: false, error: 'Forbidden: Invalid or expired token', code: 'FORBIDDEN' }), {
          status: 403,
          headers: { 'Content-Type': 'application/json' },
        })
      );
    }) as unknown as typeof fetch;

    try {
      let caught: unknown;
      try {
        await httpRequest('PUT', '/api/idmm/settings', { default_steering_prompt: '' });
      } catch (e) {
        caught = e;
      }

      expect(isAuthExpiredHttpError(caught)).toBe(true);
      expect(isHandledAuthExpiredHttpError(caught)).toBe(false);
      expect(capturedHeaders?.['x-nomi-local-trust']).toBe('local-secret');
      expect(location.hash).toBe('');
    } finally {
      globalThis.fetch = realFetch;
      restoreBrowserGlobals();
    }
  });
});
