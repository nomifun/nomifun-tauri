/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Tauri desktop-shell adapter — the Tauri-native replacement for the former
 * Electron `bridge.buildProvider/buildEmitter` IPC channels that ipcBridge.ts
 * used for OS-shell operations.
 *
 * Every operation is implemented with a Tauri v2 JS API (a plugin or
 * `@tauri-apps/api`) and is GUARDED by `isTauriRuntime()`:
 *   - In the Tauri desktop shell → the real Tauri call runs.
 *   - In the WebUI browser       → providers return a web-safe fallback and
 *                                  emitters are inert (no transport, no throw).
 *
 * Operations with no Tauri equivalent (Chrome DevTools Protocol, GPU-process
 * recovery, devtools open/close, renderer-log piping, WebUI-server lifecycle,
 * close-to-tray window behavior) are intentionally DEGRADED to safe stubs here
 * — marked `DEGRADE_STUB`. They no longer depend on the deleted `@/platform`
 * bridge. Each carries a TODO if a real Tauri port is wanted later.
 *
 * Tauri modules are loaded via dynamic `import()` inside the guarded branch so
 * the WebUI browser bundle never evaluates Tauri IPC code.
 */
import { isTauriRuntime } from './tauriRuntime';

// ---------------------------------------------------------------------------
// Channel shapes — mirror the bridge.buildProvider / bridge.buildEmitter API
// that existing ipcBridge consumers depend on.
// ---------------------------------------------------------------------------

export interface ShellProvider<Data, Params> {
  /** No-op on the renderer side; kept for API compatibility with bridge.buildProvider. */
  provider: () => void;
  invoke: (params: Params) => Promise<Data>;
}

export interface ShellEmitter<Params> {
  on: (callback: (params: Params) => void) => () => void;
  emit: (params: Params) => void;
}

/** A provider backed by a Tauri call, with a web-safe fallback for the browser. */
export function shellProvider<Data, Params = void>(
  handler: (params: Params) => Promise<Data>,
  webFallback: Data | ((params: Params) => Data | Promise<Data>)
): ShellProvider<Data, Params> {
  return {
    provider: () => {},
    invoke: async (params: Params): Promise<Data> => {
      if (isTauriRuntime()) return handler(params);
      return typeof webFallback === 'function'
        ? (webFallback as (p: Params) => Data | Promise<Data>)(params)
        : webFallback;
    },
  };
}

/** DEGRADE_STUB provider: returns a constant value in every runtime (no Tauri equivalent). */
export function stubShellProvider<Data, Params = void>(value: Data | (() => Data)): ShellProvider<Data, Params> {
  return {
    provider: () => {},
    invoke: async (): Promise<Data> => (typeof value === 'function' ? (value as () => Data)() : value),
  };
}

/** An emitter backed by a Tauri event subscription, inert in the browser. */
export function shellEmitter<Params = void>(
  subscribe: (callback: (params: Params) => void) => Promise<() => void>
): ShellEmitter<Params> {
  return {
    on: (callback: (params: Params) => void): (() => void) => {
      if (!isTauriRuntime()) return () => {};
      let unlisten: (() => void) | null = null;
      let disposed = false;
      void subscribe(callback)
        .then((un) => {
          if (disposed) un();
          else unlisten = un;
        })
        .catch(() => {});
      return () => {
        disposed = true;
        if (unlisten) unlisten();
      };
    },
    emit: () => {},
  };
}

/** DEGRADE_STUB emitter: never fires (no Tauri source for this signal). */
export function noopEmitter<Params = void>(): ShellEmitter<Params> {
  return {
    on: () => () => {},
    emit: () => {},
  };
}

// ---------------------------------------------------------------------------
// Operations (Tauri v2 JS APIs)
// ---------------------------------------------------------------------------

/** Restart the desktop shell (tauri-plugin-process). */
export async function tauriRelaunch(): Promise<void> {
  const { relaunch } = await import('@tauri-apps/plugin-process');
  await relaunch();
}

/** OS directory paths (@tauri-apps/api/path). */
export async function tauriGetPath(name: 'desktop' | 'home' | 'downloads'): Promise<string> {
  const path = await import('@tauri-apps/api/path');
  if (name === 'home') return path.homeDir();
  if (name === 'downloads') return path.downloadDir();
  return path.desktopDir();
}

// Tauri exposes no zoom *getter*; remember the last value set this session.
let lastZoomFactor = 1;
export async function tauriSetZoom(factor: number): Promise<number> {
  const { getCurrentWebview } = await import('@tauri-apps/api/webview');
  await getCurrentWebview().setZoom(factor);
  lastZoomFactor = factor;
  return factor;
}
export function tauriGetZoom(): number {
  return lastZoomFactor;
}

/** 开/关 OS 级保持唤醒(防系统休眠),走桌面 Tauri command;非桌面环境会抛错,由上层吞掉。
 *  Apply/clear the OS-level keep-awake (sleep inhibitor) via the desktop command. */
export async function tauriSetKeepAwake(enabled: boolean): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('set_keep_awake', { enabled });
}

/** 本地化原生系统托盘菜单(「显示」「退出」)。Rust 侧无法解析 i18n,创建时用英文兜底,
 *  渲染层在挂载/切换语言时调用此命令传入译文。非桌面环境会抛错,由上层吞掉。
 *  Localize the native system-tray menu labels (Show / Quit) via the desktop command. */
export async function tauriSetTrayLabels(show: string, quit: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('set_tray_labels', { show, quit });
}

/** Electron-style OpenDialog options accepted by call sites. */
export interface ShellOpenDialogOptions {
  properties?: Array<'openFile' | 'openDirectory' | 'multiSelections' | 'createDirectory' | 'showHiddenFiles'>;
  filters?: Array<{ name: string; extensions: string[] }>;
  defaultPath?: string;
}

/** Native open file/folder dialog (tauri-plugin-dialog), normalized to string[] | undefined. */
export async function tauriOpenDialog(options?: ShellOpenDialogOptions): Promise<string[] | undefined> {
  const { open } = await import('@tauri-apps/plugin-dialog');
  const props = options?.properties ?? [];
  const result = await open({
    directory: props.includes('openDirectory'),
    multiple: props.includes('multiSelections'),
    defaultPath: options?.defaultPath,
    filters: options?.filters,
  });
  if (result == null) return undefined;
  return Array.isArray(result) ? result : [result];
}

/** OS auto-launch (tauri-plugin-autostart). */
export async function tauriIsAutostartEnabled(): Promise<boolean> {
  const { isEnabled } = await import('@tauri-apps/plugin-autostart');
  return isEnabled();
}
export async function tauriSetAutostart(enabled: boolean): Promise<void> {
  const mod = await import('@tauri-apps/plugin-autostart');
  if (enabled) await mod.enable();
  else await mod.disable();
}

/** Native OS notification (tauri-plugin-notification). */
export async function tauriSendNotification(opts: { title: string; body: string; icon?: string }): Promise<void> {
  const mod = await import('@tauri-apps/plugin-notification');
  let granted = await mod.isPermissionGranted();
  if (!granted) granted = (await mod.requestPermission()) === 'granted';
  if (granted) mod.sendNotification({ title: opts.title, body: opts.body, icon: opts.icon });
}

function parseDeepLink(url: string): { action: string; params: Record<string, string> } {
  try {
    const u = new URL(url);
    const action = u.hostname || u.pathname.replace(/^\/+/, '');
    const params: Record<string, string> = {};
    u.searchParams.forEach((value, key) => {
      params[key] = value;
    });
    return { action, params };
  } catch {
    return { action: '', params: {} };
  }
}

/**
 * Subscribe to `nomifun://` deep links. The Rust shell (apps/desktop/src/main.rs)
 * forwards opened URLs on the Tauri event `deep-link://received` as a string[].
 */
export async function subscribeDeepLink(
  callback: (payload: { action: string; params: Record<string, string> }) => void
): Promise<() => void> {
  const { listen } = await import('@tauri-apps/api/event');
  return listen<string[]>('deep-link://received', (event) => {
    for (const url of event.payload ?? []) callback(parseDeepLink(url));
  });
}

// ---- window controls (@tauri-apps/api/window) ----

export async function tauriWindowMinimize(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().minimize();
}
export async function tauriWindowMaximize(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().maximize();
}
export async function tauriWindowUnmaximize(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().unmaximize();
}
export async function tauriWindowToggleMaximize(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().toggleMaximize();
}
export async function tauriWindowClose(): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().close();
}
export async function tauriWindowIsMaximized(): Promise<boolean> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  return getCurrentWindow().isMaximized();
}
export async function subscribeWindowMaximized(
  callback: (payload: { is_maximized: boolean }) => void
): Promise<() => void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  const win = getCurrentWindow();
  return win.onResized(() => {
    void win.isMaximized().then((is_maximized) => callback({ is_maximized }));
  });
}

// ---- WebUI / LAN remote-access lifecycle (Tauri commands + status event) ----

/** Invoke a Tauri command via `@tauri-apps/api/core`. */
async function invokeCommand<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke<T>(cmd, args);
}

/** Current WebUI/LAN serving status (backend `webui_get_status`). */
export function tauriWebuiGetStatus<T>(): Promise<T> {
  return invokeCommand<T>('webui_get_status');
}

/** Start LAN serving (backend `webui_start`). Returns the resulting status. */
export function tauriWebuiStart<T>(): Promise<T> {
  return invokeCommand<T>('webui_start');
}

/** Stop LAN serving (backend `webui_stop`). Returns the resulting status. */
export function tauriWebuiStop<T>(): Promise<T> {
  return invokeCommand<T>('webui_stop');
}

/**
 * Subscribe to backend-emitted WebUI/LAN status changes
 * (`apps/desktop/src/main.rs` forwards them on `webui://status-changed`).
 */
export async function subscribeWebuiStatus<T>(callback: (status: T) => void): Promise<() => void> {
  const { listen } = await import('@tauri-apps/api/event');
  return listen<T>('webui://status-changed', (event) => callback(event.payload));
}
