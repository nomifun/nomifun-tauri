/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useCallback, useEffect, useState } from 'react';
import { ipcBridge } from '@/common';
import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import type { TerminalId } from '@/common/types/ids';
import { emitter } from '@/renderer/utils/emitter';

/**
 * Live list of standalone, user-owned terminal sessions for the global
 * sidebar. Conversation-owned terminals are intentionally excluded: their
 * lifecycle belongs to the conversation that created them and they are shown
 * in that conversation's right-hand terminal panel instead.
 */
export function useTerminalSessions() {
  const [sessions, setSessions] = useState<ITerminalSession[]>([]);
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const list = await ipcBridge.terminal.list.invoke();
      setSessions(Array.isArray(list) ? list : []);
    } catch {
      setSessions([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();

    const offCreated = ipcBridge.terminal.onCreated.on((s) => {
      if (s.owner_conversation_id) return;
      setSessions((prev) =>
        prev.some((p) => p.terminal_id === s.terminal_id) ? prev : [s, ...prev],
      );
    });
    const offUpdated = ipcBridge.terminal.onUpdated.on((s) => {
      setSessions((prev) => {
        if (s.owner_conversation_id) {
          return prev.filter((p) => p.terminal_id !== s.terminal_id);
        }
        return prev.map((p) => (p.terminal_id === s.terminal_id ? s : p));
      });
    });
    const offRemoved = ipcBridge.terminal.onRemoved.on((evt) => {
      setSessions((prev) => prev.filter((p) => p.terminal_id !== evt.terminal_id));
    });
    const offExit = ipcBridge.terminal.onExit.on((evt) => {
      setSessions((prev) =>
        prev.map((p) =>
          p.terminal_id === evt.terminal_id
            ? { ...p, last_status: 'exited', exit_code: evt.exit_code }
            : p,
        ),
      );
    });
    const offRefresh = (): void => {
      void refresh();
    };
    emitter.on('terminal.list.refresh', offRefresh);
    const offReconnected = ipcBridge.terminal.onReconnected.on(() => {
      void refresh();
    });

    return () => {
      offCreated();
      offUpdated();
      offRemoved();
      offExit();
      offReconnected();
      emitter.off('terminal.list.refresh', offRefresh);
    };
  }, [refresh]);

  const removeSession = useCallback(async (id: TerminalId) => {
    await ipcBridge.terminal.remove.invoke({ terminal_id: id });
    setSessions((prev) => prev.filter((p) => p.terminal_id !== id));
  }, []);

  return { sessions, loading, refresh, removeSession };
}
