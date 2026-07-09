import { describe, expect, test } from 'bun:test';

import { isTauriRuntime } from './tauriRuntime';

const originalWindow = globalThis.window;

const withWindow = (value: unknown, run: () => void): void => {
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value,
  });

  try {
    run();
  } finally {
    if (typeof originalWindow === 'undefined') {
      Reflect.deleteProperty(globalThis, 'window');
    } else {
      Object.defineProperty(globalThis, 'window', {
        configurable: true,
        value: originalWindow,
      });
    }
  }
};

describe('isTauriRuntime', () => {
  test('recognizes the desktop shell by injected backend port', () => {
    withWindow({ __backendPort: 18188 }, () => {
      expect(isTauriRuntime()).toBe(true);
    });
  });

  test('recognizes Tauri globals when present', () => {
    withWindow({ __TAURI_INTERNALS__: {} }, () => {
      expect(isTauriRuntime()).toBe(true);
    });

    withWindow({ __TAURI__: {} }, () => {
      expect(isTauriRuntime()).toBe(true);
    });

    withWindow({ isTauri: true }, () => {
      expect(isTauriRuntime()).toBe(true);
    });
  });

  test('does not treat a plain WebUI browser as Tauri', () => {
    withWindow({}, () => {
      expect(isTauriRuntime()).toBe(false);
    });
  });
});
