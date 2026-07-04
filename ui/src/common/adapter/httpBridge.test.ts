/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { httpRequest, isBackendHttpError } from './httpBridge';

const realFetch = globalThis.fetch;

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
});
