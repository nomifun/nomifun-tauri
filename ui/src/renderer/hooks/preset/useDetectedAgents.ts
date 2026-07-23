import { ipcBridge } from '@/common';
import type { AgentMetadata } from '@/renderer/utils/model/agentTypes';
import type { AgentId } from '@/common/types/ids';
import { DETECTED_AGENTS_SWR_KEY, fetchDetectedAgents } from '@/renderer/utils/model/agentTypes';
import { useCallback, useMemo } from 'react';
import useSWR, { mutate } from 'swr';

export type AvailableBackend = {
  id: AgentId;
  name: string;
  isExtension?: boolean;
};

/**
 * Provides detected execution engines for backend selectors (e.g. PresetEditDrawer).
 * Excludes presets — those live in the backend catalog
 * (`ipcBridge.presets.list`).
 *
 * Returns `availableBackends` (simplified shape for Select dropdowns)
 * and `refreshAgentDetection` to trigger a re-scan.
 */
export const useDetectedAgents = () => {
  const { data: rawAgents = [] } = useSWR<AgentMetadata[]>(DETECTED_AGENTS_SWR_KEY, fetchDetectedAgents);

  const availableBackends = useMemo<AvailableBackend[]>(
    () =>
      rawAgents
        .filter((a) => a.agent_type !== 'remote')
        .map((a) => ({
          // Presets reference the canonical AgentMetadata agent_id, never a backend slug.
          id: a.agent_id,
          name: a.name,
          isExtension: a.agent_source === 'extension',
        })),
    [rawAgents]
  );

  const refreshAgentDetection = useCallback(async () => {
    try {
      await ipcBridge.acpConversation.refreshCustomAgents.invoke();
      await mutate(DETECTED_AGENTS_SWR_KEY);
    } catch {
      // ignore
    }
  }, []);

  return {
    availableBackends,
    refreshAgentDetection,
  };
};
