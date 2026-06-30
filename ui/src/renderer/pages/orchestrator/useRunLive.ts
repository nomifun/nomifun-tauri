/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { TRunDetail } from '@/common/types/orchestrator/orchestratorTypes';
import { useCallback, useEffect, useState } from 'react';

/**
 * Live view of a single orchestration run. Fetches the full run detail
 * (`run` + plan tasks/deps + assignments) over REST, then refetches whenever
 * any of the five run-engine WebSocket events for THIS run arrives. Every event
 * payload carries `run_id`, so we filter on it before refetching.
 *
 * Passing `undefined` clears the detail and skips subscriptions (e.g. when no
 * run is selected). The realtime refresh is a simple debounce-free refetch;
 * optimistic in-place patching is intentionally deferred (see Task brief).
 */
export function useRunLive(runId: string | undefined): {
  detail: TRunDetail | null;
  loading: boolean;
  refetch: () => Promise<void>;
} {
  const [detail, setDetail] = useState<TRunDetail | null>(null);
  const [loading, setLoading] = useState(false);

  const refetch = useCallback(async () => {
    if (!runId) {
      setDetail(null);
      return;
    }
    setLoading(true);
    try {
      const result = await ipcBridge.orchestrator.runs.get.invoke({ id: runId });
      setDetail(result ?? null);
    } catch (err) {
      console.error('[useRunLive] Failed to fetch run detail:', err);
      setDetail(null);
    } finally {
      setLoading(false);
    }
  }, [runId]);

  // Initial fetch (and clear when runId becomes undefined).
  useEffect(() => {
    void refetch();
  }, [refetch]);

  // Refetch when any run-engine event for this run arrives. All five payloads
  // carry `run_id`; we filter on it and collect every unsubscribe for cleanup.
  useEffect(() => {
    if (!runId) return;
    const onRunEvent = (e: { run_id: string }) => {
      if (e.run_id === runId) {
        void refetch();
      }
    };
    const unsubs = [
      ipcBridge.orchestrator.runEvents.statusChanged.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.planUpdated.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.completed.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.taskStatusChanged.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.taskAssigned.on(onRunEvent),
    ];
    return () => {
      for (const unsub of unsubs) {
        unsub();
      }
    };
  }, [runId, refetch]);

  return { detail, loading, refetch };
}
