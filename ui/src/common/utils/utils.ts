/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Short random hex string for *local, throwaway* UI keys (React list keys,
 * render-item keys, component instance ids). NOT for stable business IDs: anything
 * persisted by / sent to the backend should use the bare UUIDv7 helper from
 * ./uuidv7 or a backend-issued business ID.
 */
export const uuid = (length = 8) => {
  try {
    // globalThis.crypto is available in all modern browsers and Node.js 19+
    const crypto = globalThis.crypto;
    if (crypto) {
      if (typeof crypto.randomUUID === 'function' && length >= 36) {
        return crypto.randomUUID();
      }
      if (typeof crypto.getRandomValues === 'function') {
        const bytes = new Uint8Array(Math.ceil(length / 2));
        crypto.getRandomValues(bytes);
        return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0'))
          .join('')
          .slice(0, length);
      }
    }
  } catch {
    // Fallback without crypto
  }

  // Monotonic fallback without cryptographically secure randomness
  const base = Date.now().toString(36);
  return (base + base).slice(0, length);
};

export const parseError = (error: unknown): string => {
  if (typeof error === 'object' && error !== null) {
    const err = error as { backendMessage?: unknown; msg?: unknown; message?: unknown };
    if (typeof err.msg === 'string') return err.msg;
    if (typeof err.backendMessage === 'string' && err.backendMessage.trim()) return err.backendMessage;
    if (typeof err.message === 'string') return err.message;
  }

  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;

  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
};

/**
 * 根据语言代码解析为标准化的区域键
 * Resolve language code to standardized locale key
 */
export const resolveLocaleKey = (language: string): 'zh-CN' | 'en-US' => {
  const lang = language.toLowerCase();
  if (lang.startsWith('zh')) return 'zh-CN';
  return 'en-US';
};
