/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import useSWR, { type SWRConfiguration } from 'swr';
import { ipcBridge } from '@/common';
import type { TRun } from '@/common/types/orchestrator/orchestratorTypes';

/**
 * SWR hooks for the 「智能编排」(orchestration) page. Runs are now created from
 * conversations, so this page is a read-only Run-history library: it lists the
 * current user's runs (all workspaces + ad-hoc) via `runs.listMine`. Like the
 * rest of the page we never poll — realtime freshness for an open run comes
 * from `useRunLive` (per-run); the list is refreshed only via explicit
 * `mutate()`.
 */
export const ORCH_MY_RUNS_SWR_KEY = 'orchestrator.runs.mine';

const ORCH_SWR_OPTIONS: SWRConfiguration = {
  revalidateOnFocus: false,
  revalidateOnReconnect: false,
  shouldRetryOnError: false,
};

/**
 * Load every run owned by the current user (all workspaces + ad-hoc/
 * workspace-less runs), newest first — the read path for the read-only
 * Run-history library.
 */
export function useMyRuns(): {
  runs: TRun[];
  isLoading: boolean;
  error: unknown;
  mutate: () => void;
} {
  const { data, isLoading, error, mutate } = useSWR<TRun[]>(
    ORCH_MY_RUNS_SWR_KEY,
    async () => (await ipcBridge.orchestrator.runs.listMine.invoke()) ?? [],
    ORCH_SWR_OPTIONS
  );
  return {
    runs: data ?? [],
    isLoading,
    error,
    mutate: () => {
      void mutate();
    },
  };
}
