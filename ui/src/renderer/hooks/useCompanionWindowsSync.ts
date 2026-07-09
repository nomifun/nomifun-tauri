/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useEffect } from 'react';
import { ipcBridge } from '@/common';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';

/** Collapse event bursts (e.g. drag-save config-updated echoes) into one sync. */
const SYNC_DEBOUNCE_MS = 500;

/** In-process subscribers (the mounted hook registers one). Lets a deliberate
 *  user action — the 总览 enable/disable switch — force an IMMEDIATE window
 *  reconcile without waiting on the WS `companion.config-updated` echo. That echo
 *  is lossy on the desktop (the WS reconnect uses backoff with no replay/heartbeat),
 *  so a dropped event left the pet not showing/hiding until a manual reload
 *  ("点隐藏/显示按钮不及时生效，要右键刷新"). */
const immediateSyncListeners = new Set<() => void>();
export function requestCompanionWindowSync(): void {
  for (const listener of immediateSyncListeners) listener();
}

/**
 * Main-window owner of the native desktop-companion window set (multi-companion, spec
 * §4.6). On mount and on companion.created / companion.deleted / companion.config-updated
 * (debounced), reads the companion registry and asks the Tauri shell to reconcile
 * one `companion-{companion_id}` window per enabled companion via the `sync_companion_windows`
 * command. No-op outside the Tauri desktop shell (WebUI browser).
 */
export function useCompanionWindowsSync(): void {
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const sync = async (): Promise<void> => {
      try {
        const companions = await ipcBridge.companion.listCompanions.invoke();
        if (disposed) return;
        const specs = companions.map((p) => ({ companion_id: p.id, enabled: Boolean(p.appearance.companion_enabled) }));
        const { invoke } = await import('@tauri-apps/api/core');
        await invoke('sync_companion_windows', { specs });
      } catch (e) {
        // Never throw into React — a failed sync self-heals on the next companion event.
        console.warn('sync_companion_windows failed:', e);
      }
    };

    const schedule = (): void => {
      if (disposed) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => void sync(), SYNC_DEBOUNCE_MS);
    };

    void sync();
    // Deliberate in-process requests (the 总览 enable/disable switch) reconcile
    // IMMEDIATELY — they are user toggles, not echo bursts, so they must not
    // depend on the lossy WS event. Also resync when the main window regains
    // focus, as a catch-all for any event missed while it was backgrounded.
    const syncNow = (): void => {
      if (!disposed) void sync();
    };
    immediateSyncListeners.add(syncNow);
    window.addEventListener('focus', schedule);
    const unsubs = [
      ipcBridge.companion.onCompanionCreated.on(schedule),
      ipcBridge.companion.onCompanionDeleted.on(schedule),
      ipcBridge.companion.onConfigUpdated.on(schedule),
    ];
    return () => {
      disposed = true;
      if (timer) clearTimeout(timer);
      immediateSyncListeners.delete(syncNow);
      window.removeEventListener('focus', schedule);
      for (const unsub of unsubs) unsub();
    };
  }, []);
}

export default useCompanionWindowsSync;
