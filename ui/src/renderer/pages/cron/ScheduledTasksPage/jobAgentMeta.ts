/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ICronJob } from '@/common/adapter/ipcBridge';
import { getAgentLogo } from '@renderer/utils/model/agentLogo';
import type { AgentMetadata } from '@renderer/utils/model/agentTypes';

function normalizeAgentBackend(agent: string | undefined): string | undefined {
  if (!agent) return undefined;
  return agent.replace(/^cli:/, '').replace(/^preset:/, '');
}

/**
 * Resolve the display name and logo for a cron job's agent.
 *
 * ACP jobs store the literal string "acp" in `agent_type`; the real vendor id
 * (claude/gemini/codex/…) and the human-readable label live in `agent_config`.
 * Non-ACP agents (nomi, remote, nanobot, openclaw-gateway, …) use
 * `agent_type` directly. Nomi's provider_id is a model selection and is not
 * used to resolve the agent logo.
 */
export function getJobAgentMeta(job: ICronJob, cliAgents: AgentMetadata[]): { name?: string; logo?: string | null } {
  const rawType = normalizeAgentBackend(job.metadata.agent_type);
  if (!rawType) return {};

  if (rawType === 'acp') {
    const backend = job.metadata.agent_config?.backend;
    const detected = backend ? cliAgents.find((a) => (a.backend || a.agent_type) === backend) : undefined;
    return {
      name: detected?.name || job.metadata.agent_config?.name || backend || rawType,
      logo: getAgentLogo(backend),
    };
  }

  const detected = cliAgents.find((a) => (a.backend || a.agent_type) === rawType);
  return {
    name: detected?.name || rawType,
    logo: getAgentLogo(rawType),
  };
}
