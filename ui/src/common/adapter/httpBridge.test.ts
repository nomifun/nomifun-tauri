/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  AUTH_EXPIRED_EVENT,
  httpPost,
  httpRequest,
  isAuthExpiredHttpError,
  isBackendHttpError,
  isHandledAuthExpiredHttpError,
  wsEmitter,
} from './httpBridge';

const realFetch = globalThis.fetch;

const realWindow = (globalThis as { window?: Window }).window;
const realDocument = (globalThis as { document?: Document }).document;
const realWebSocket = globalThis.WebSocket;

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

function restoreWebSocketGlobal() {
  if (realWebSocket === undefined) {
    delete (globalThis as { WebSocket?: typeof WebSocket }).WebSocket;
  } else {
    globalThis.WebSocket = realWebSocket;
  }
}

describe('httpRequest client deadline + network-failure diagnosis', () => {
  test('forces mutable API reads past every host WebView HTTP cache', async () => {
    const requestInits: RequestInit[] = [];
    globalThis.fetch = ((_url: string, init?: RequestInit) => {
      if (init) requestInits.push(init);
      return Promise.resolve(
        new Response(JSON.stringify({ success: true, data: { items: [] } }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      );
    }) as unknown as typeof fetch;

    try {
      await httpRequest('GET', '/api/conversations/conv_test/messages?page=0&page_size=10000');
      await httpRequest('POST', '/api/conversations/conv_test/messages', { content: 'hello' });
      expect(requestInits[0]?.cache).toBe('no-store');
      expect(requestInits[1]?.cache).toBeUndefined();
    } finally {
      globalThis.fetch = realFetch;
    }
  });

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

  test('httpPost forwards a deadline for idempotent timed mutations', async () => {
    globalThis.fetch = ((_url: string, init?: RequestInit) =>
      new Promise((_resolve, reject) => {
        init?.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')));
      })) as unknown as typeof fetch;
    try {
      const resize = httpPost<void, { cols: number; rows: number }>(
        '/api/terminals/test/resize',
        (size) => size,
        { timeoutMs: 10 }
      );
      let message = '';
      try {
        await resize.invoke({ cols: 100, rows: 30 });
      } catch (error) {
        message = error instanceof Error ? error.message : String(error);
      }
      expect(message.toLowerCase().includes('timed out')).toBe(true);
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

describe('httpBridge WebSocket heartbeat', () => {
  test('handles application heartbeat internally', () => {
    const realSetTimeout = globalThis.setTimeout;
    const scheduledReconnects: Array<() => void> = [];
    class FakeWebSocket {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSING = 2;
      static readonly CLOSED = 3;
      static readonly instances: FakeWebSocket[] = [];

      readyState = FakeWebSocket.OPEN;
      readonly sent: string[] = [];
      private readonly listeners = new Map<string, Array<(event: unknown) => void>>();

      constructor(..._args: unknown[]) {
        FakeWebSocket.instances.push(this);
      }

      addEventListener(type: string, listener: (event: unknown) => void) {
        const listeners = this.listeners.get(type) ?? [];
        listeners.push(listener);
        this.listeners.set(type, listeners);
      }

      send(data: string) {
        this.sent.push(data);
      }

      close() {
        this.readyState = FakeWebSocket.CLOSED;
      }

      dispatch(type: string, event: unknown) {
        for (const listener of this.listeners.get(type) ?? []) {
          listener(event);
        }
      }
    }

    installBrowserGlobals({
      location: {
        protocol: 'http:',
        host: 'localhost:25808',
        pathname: '/sessions',
        hash: '',
      } as Location,
    });
    globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
    globalThis.setTimeout = ((callback: () => void) => {
      scheduledReconnects.push(callback);
      return scheduledReconnects.length as unknown as ReturnType<typeof setTimeout>;
    }) as typeof setTimeout;

    let unsubscribe = () => {};
    let socket: FakeWebSocket | undefined;
    try {
      const dispatched: unknown[] = [];
      unsubscribe = wsEmitter<unknown>('ping').on((payload) => dispatched.push(payload));
      socket = FakeWebSocket.instances[0];
      if (!socket) throw new Error('httpBridge did not create a WebSocket');

      socket.dispatch('message', {
        data: JSON.stringify({ name: 'ping', data: { timestamp: 123 } }),
      });

      expect(socket.sent.length).toBe(1);
      const pong = JSON.parse(socket.sent[0]) as { name: string; data: { timestamp: unknown } };
      expect(pong.name).toBe('pong');
      expect(typeof pong.data.timestamp).toBe('number');
      expect(dispatched.length).toBe(0);

      unsubscribe();
      unsubscribe = () => {};
      socket.dispatch('close', { code: 1000, reason: 'test cleanup' });
      expect(scheduledReconnects.length).toBe(0);
    } finally {
      unsubscribe();
      socket?.close();
      globalThis.setTimeout = realSetTimeout;
      restoreWebSocketGlobal();
      restoreBrowserGlobals();
    }
  });

  test('retries when the initial WebSocket constructor throws', () => {
    const realSetTimeout = globalThis.setTimeout;
    const realConsoleError = console.error;
    const scheduledReconnects: Array<() => void> = [];

    class ThrowOnceWebSocket {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSING = 2;
      static readonly CLOSED = 3;
      static attempts = 0;
      static readonly instances: ThrowOnceWebSocket[] = [];

      readyState = ThrowOnceWebSocket.OPEN;
      private readonly listeners = new Map<string, Array<(event: unknown) => void>>();

      constructor(..._args: unknown[]) {
        ThrowOnceWebSocket.attempts += 1;
        if (ThrowOnceWebSocket.attempts === 1) {
          throw new Error('constructor failed');
        }
        ThrowOnceWebSocket.instances.push(this);
      }

      addEventListener(type: string, listener: (event: unknown) => void) {
        const listeners = this.listeners.get(type) ?? [];
        listeners.push(listener);
        this.listeners.set(type, listeners);
      }

      send(_data: string) {}

      close() {
        this.readyState = ThrowOnceWebSocket.CLOSED;
      }

      dispatch(type: string, event: unknown) {
        for (const listener of this.listeners.get(type) ?? []) listener(event);
      }
    }

    installBrowserGlobals({
      location: {
        protocol: 'http:',
        host: 'localhost:25808',
        pathname: '/sessions',
        hash: '',
      } as Location,
    });
    globalThis.WebSocket = ThrowOnceWebSocket as unknown as typeof WebSocket;
    globalThis.setTimeout = ((callback: () => void) => {
      scheduledReconnects.push(callback);
      return scheduledReconnects.length as unknown as ReturnType<typeof setTimeout>;
    }) as typeof setTimeout;
    console.error = () => {};

    let unsubscribe = () => {};
    try {
      unsubscribe = wsEmitter<unknown>('message.stream').on(() => {});
      expect(scheduledReconnects.length).toBe(1);

      scheduledReconnects[0]?.();
      expect(ThrowOnceWebSocket.attempts).toBe(2);
      expect(ThrowOnceWebSocket.instances.length).toBe(1);
    } finally {
      unsubscribe();
      for (const socket of ThrowOnceWebSocket.instances) {
        socket.dispatch('close', { code: 1000, reason: 'test cleanup' });
        socket.close();
      }
      globalThis.setTimeout = realSetTimeout;
      console.error = realConsoleError;
      restoreWebSocketGlobal();
      restoreBrowserGlobals();
    }
  });

  test('cancels a queued reconnect when the final listener unsubscribes', () => {
    const realSetTimeout = globalThis.setTimeout;
    const realClearTimeout = globalThis.clearTimeout;
    const scheduledReconnects: Array<() => void> = [];
    const scheduledDelays: number[] = [];
    const clearedHandles: unknown[] = [];

    class RecordingWebSocket {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSING = 2;
      static readonly CLOSED = 3;
      static readonly instances: RecordingWebSocket[] = [];

      readyState = RecordingWebSocket.OPEN;
      private readonly listeners = new Map<string, Array<(event: unknown) => void>>();

      constructor(..._args: unknown[]) {
        RecordingWebSocket.instances.push(this);
      }

      addEventListener(type: string, listener: (event: unknown) => void) {
        const listeners = this.listeners.get(type) ?? [];
        listeners.push(listener);
        this.listeners.set(type, listeners);
      }

      send(_data: string) {}

      close() {
        this.readyState = RecordingWebSocket.CLOSED;
      }

      dispatch(type: string, event: unknown) {
        for (const listener of this.listeners.get(type) ?? []) listener(event);
      }
    }

    installBrowserGlobals({
      location: {
        protocol: 'http:',
        host: 'localhost:25808',
        pathname: '/sessions',
        hash: '',
      } as Location,
    });
    globalThis.WebSocket = RecordingWebSocket as unknown as typeof WebSocket;
    globalThis.setTimeout = ((callback: () => void, delay?: number) => {
      scheduledReconnects.push(callback);
      scheduledDelays.push(delay ?? 0);
      return scheduledReconnects.length as unknown as ReturnType<typeof setTimeout>;
    }) as typeof setTimeout;
    globalThis.clearTimeout = ((handle: unknown) => {
      clearedHandles.push(handle);
    }) as typeof clearTimeout;

    let unsubscribe = () => {};
    let unsubscribeAgain = () => {};
    try {
      unsubscribe = wsEmitter<unknown>('turn.completed').on(() => {});
      const socket = RecordingWebSocket.instances[0];
      if (!socket) throw new Error('httpBridge did not create a WebSocket');

      socket.dispatch('close', { code: 1006, reason: 'network lost' });
      expect(scheduledReconnects.length).toBe(1);

      unsubscribe();
      unsubscribe = () => {};
      expect(clearedHandles.length).toBe(1);

      scheduledReconnects[0]?.();
      expect(RecordingWebSocket.instances.length).toBe(1);

      unsubscribeAgain = wsEmitter<unknown>('turn.completed').on(() => {});
      const replacement = RecordingWebSocket.instances[1];
      if (!replacement) throw new Error('httpBridge did not start a fresh WebSocket lifecycle');
      replacement.dispatch('close', { code: 1006, reason: 'network lost again' });
      expect(scheduledDelays.at(-1)).toBe(1000);
    } finally {
      unsubscribe();
      unsubscribeAgain();
      scheduledReconnects[0]?.();
      for (const socket of RecordingWebSocket.instances) {
        socket.dispatch('close', { code: 1000, reason: 'test cleanup' });
        socket.close();
      }
      globalThis.setTimeout = realSetTimeout;
      globalThis.clearTimeout = realClearTimeout;
      restoreWebSocketGlobal();
      restoreBrowserGlobals();
    }
  });
});
