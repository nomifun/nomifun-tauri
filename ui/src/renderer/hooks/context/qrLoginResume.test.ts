/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { QR_LOGIN_RESUME_KEY, consumeQrLoginResume } from './qrLoginResume';

const installSessionStorage = () => {
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  const store = new Map<string, string>();
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    writable: true,
    value: {
      sessionStorage: {
        getItem: (key: string) => store.get(key) ?? null,
        setItem: (key: string, value: string) => {
          store.set(key, value);
        },
        removeItem: (key: string) => {
          store.delete(key);
        },
      },
    },
  });

  return {
    store,
    restore: () => {
      if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow);
      else Reflect.deleteProperty(globalThis, 'window');
    },
  };
};

describe('consumeQrLoginResume', () => {
  test('returns and removes a fresh QR login user', () => {
    const { store, restore } = installSessionStorage();
    const userId = 'user_0190f5fe-7c00-7a00-8000-000000000001';
    try {
      store.set(
        QR_LOGIN_RESUME_KEY,
        JSON.stringify({
          at: 1_000,
          user: { id: userId, username: 'admin' },
        })
      );

      expect(consumeQrLoginResume(2_000)).toEqual({ id: userId, username: 'admin' });
      expect(store.has(QR_LOGIN_RESUME_KEY)).toBe(false);
    } finally {
      restore();
    }
  });

  test('ignores expired QR login resume data', () => {
    const { store, restore } = installSessionStorage();
    const userId = 'user_0190f5fe-7c00-7a00-8000-000000000001';
    try {
      store.set(
        QR_LOGIN_RESUME_KEY,
        JSON.stringify({
          at: 1_000,
          user: { id: userId, username: 'admin' },
        })
      );

      expect(consumeQrLoginResume(40_000)).toBe(null);
      expect(store.has(QR_LOGIN_RESUME_KEY)).toBe(false);
    } finally {
      restore();
    }
  });
});
