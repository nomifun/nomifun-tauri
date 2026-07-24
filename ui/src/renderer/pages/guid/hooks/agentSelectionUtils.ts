/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { configService } from '@/common/config/configService';
import type { AgentSource } from '@/renderer/utils/model/agentTypes';
import type { ProviderId } from '@/common/types/ids';

/** Save preferred mode to the agent's own config key */
export async function savePreferredMode(agentKey: string, mode: string): Promise<void> {
  try {
    if (agentKey === 'nomi') {
      const config = configService.get('nomi.config');
      await configService.set('nomi.config', { ...config, preferredMode: mode });
    } else if (agentKey !== 'custom') {
      const config = configService.get('acp.config');
      const backendConfig = config?.[agentKey as string] || {};
      await configService.set('acp.config', { ...config, [agentKey]: { ...backendConfig, preferredMode: mode } });
    }
  } catch {
    /* silent */
  }
}

/** Save preferred model ID to the agent's acp.config key */
export async function savePreferredModelId(agentKey: string, model_id: string): Promise<void> {
  try {
    const config = configService.get('acp.config');
    const backendConfig = config?.[agentKey as string] || {};
    await configService.set('acp.config', { ...config, [agentKey]: { ...backendConfig, preferredModelId: model_id } });
  } catch {
    /* silent */
  }
}

/** Save default nomi provider/model so the Guid page restores it next session. */
export async function saveNomiDefaultModel(provider_id: ProviderId, use_model: string): Promise<void> {
  try {
    await configService.set('nomi.defaultModel', { provider_id, model: use_model });
  } catch {
    /* silent */
  }
}

/**
 * Get agent key for selection.
 *
 * Rows that are row-scoped (custom ACP / remote agents) use `agent_id` directly
 * as the key — no namespace prefix. Builtin / internal agents keep `backend` or
 * `agent_type` as the key since there is only one row per type.
 *
 * Note: preset *presets* (not agents) still use a `preset:<presetId>`
 * form produced inline by `PresetSelectionArea`. That is a separate
 * selection path that points at the backend-merged preset catalog, not
 * `AgentRegistry`.
 */
export const getAgentKey = (agent: {
  agent_type: string;
  agent_source?: AgentSource;
  backend?: string;
  /** Named wire identity on AgentMetadata before it enters a UI aggregate. */
  agent_id?: string;
  /** Local identity slot used by the mixed AvailableAgent display aggregate. */
  id?: string;
  is_preset?: boolean;
}): string => {
  const rowScoped = agent.agent_type === 'remote' || agent.agent_source === 'custom';
  const rowIdentity = agent.agent_id ?? agent.id;
  if (rowScoped && rowIdentity) return rowIdentity;
  return agent.backend || agent.agent_type;
};
