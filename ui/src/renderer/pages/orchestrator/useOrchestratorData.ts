/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import useSWR, { type SWRConfiguration } from 'swr';
import { ipcBridge } from '@/common';
import type { TFleet, TOrchWorkspace, TRun } from '@/common/types/orchestrator/orchestratorTypes';

/**
 * SWR hooks for the 「智能编排」(orchestration) page. Fleets and workspaces are
 * local application state fetched over REST (`ipcBridge.orchestrator.*`). We
 * keep them stable after the initial load and refresh only through explicit
 * `mutate()` calls after CRUD — matching the provider-list convention.
 */
export const ORCH_FLEETS_SWR_KEY = 'orchestrator.fleets';
export const ORCH_WORKSPACES_SWR_KEY = 'orchestrator.workspaces';

const ORCH_SWR_OPTIONS: SWRConfiguration = {
  revalidateOnFocus: false,
  revalidateOnReconnect: false,
  shouldRetryOnError: false,
};

const fetchFleets = async (): Promise<TFleet[]> => {
  return (await ipcBridge.orchestrator.fleets.list.invoke()) ?? [];
};

const fetchWorkspaces = async (): Promise<TOrchWorkspace[]> => {
  return (await ipcBridge.orchestrator.workspaces.list.invoke()) ?? [];
};

/** Load the persisted fleets (key `'orchestrator.fleets'`). */
export const useFleets = () => {
  return useSWR<TFleet[]>(ORCH_FLEETS_SWR_KEY, fetchFleets, ORCH_SWR_OPTIONS);
};

/** Load the persisted orchestration workspaces (key `'orchestrator.workspaces'`). */
export const useWorkspaces = () => {
  return useSWR<TOrchWorkspace[]>(ORCH_WORKSPACES_SWR_KEY, fetchWorkspaces, ORCH_SWR_OPTIONS);
};

/**
 * Load the runs for a single orchestration workspace. SWR key
 * `orchestrator/runs/${workspaceId}`; passing `undefined` yields a null key so
 * SWR skips fetching (e.g. before a workspace is selected). Like the fleet/
 * workspace hooks we never poll — realtime freshness comes from `useRunLive`
 * (per-run) and explicit `mutate()` after create/cancel.
 */
export function useRuns(workspaceId: string | undefined): {
  runs: TRun[];
  isLoading: boolean;
  error: unknown;
  mutate: () => void;
} {
  const { data, isLoading, error, mutate } = useSWR<TRun[]>(
    workspaceId ? `orchestrator/runs/${workspaceId}` : null,
    async () => (await ipcBridge.orchestrator.runs.list.invoke({ workspace_id: workspaceId! })) ?? [],
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
