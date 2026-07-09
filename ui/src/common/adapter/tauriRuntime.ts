/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Single source of truth for "can this renderer call Tauri APIs?".
 *
 * The desktop shell always injects `window.__backendPort` from Rust before the
 * app bundle runs. Tauri also exposes runtime globals, but those have proven
 * less stable across WebView engines and platform builds. Linux WebKitGTK in
 * particular can render the desktop UI while missing the globals checked by the
 * old guards, causing file/dialog actions to silently fall back to WebUI paths.
 */
export const isTauriRuntime = (): boolean => {
  if (typeof window === 'undefined') return false;
  const w = window as Window & {
    isTauri?: boolean;
    __backendPort?: number;
    __TAURI__?: unknown;
    __TAURI_INTERNALS__?: unknown;
  };
  return Boolean(w.isTauri) || '__TAURI_INTERNALS__' in w || '__TAURI__' in w || typeof w.__backendPort === 'number';
};
