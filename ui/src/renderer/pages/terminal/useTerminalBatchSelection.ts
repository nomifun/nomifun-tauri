/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useCallback, useState } from 'react';
import { ipcBridge } from '@/common';
import type { TerminalId } from '@/common/types/ids';

/**
 * Batch select/delete for terminal sessions, mirroring the conversation
 * batch-selection pattern. `deleteSelected` removes each selected session via
 * the terminal DELETE endpoint.
 */
export function useTerminalBatchSelection() {
  const [batchMode, setBatchMode] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<TerminalId>>(new Set());

  const toggleBatchMode = useCallback(() => {
    setBatchMode((on) => {
      if (on) setSelectedIds(new Set());
      return !on;
    });
  }, []);

  const toggleSelected = useCallback((id: TerminalId) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const selectAll = useCallback((ids: TerminalId[]) => {
    setSelectedIds(new Set(ids));
  }, []);

  const clear = useCallback(() => setSelectedIds(new Set()), []);

  const deleteSelected = useCallback(async (): Promise<number> => {
    const ids = Array.from(selectedIds);
    let removed = 0;
    for (const id of ids) {
      try {
        await ipcBridge.terminal.remove.invoke({ terminal_id: id });
        removed += 1;
      } catch {
        /* best-effort; continue */
      }
    }
    setSelectedIds(new Set());
    setBatchMode(false);
    return removed;
  }, [selectedIds]);

  return {
    batchMode,
    selectedIds,
    toggleBatchMode,
    toggleSelected,
    selectAll,
    clear,
    deleteSelected,
  };
}
