/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TFleetMember } from '@/common/types/orchestrator/orchestratorTypes';
import { resolveAgentLogo } from '@/renderer/utils/model/agentLogo';

/**
 * A run's fleet snapshot carries only the member's `agent_id` (a backend slug,
 * e.g. `claude` / `codex`), an optional `model`, and an optional `role_hint` —
 * NOT a full AgentMetadata record. So the friendliest stable label we can build
 * without an extra lookup is the agent id plus the model (when present). This is
 * still far better than showing the raw `fmem_…` member uuid.
 *
 * Examples: `claude · sonnet-4.5`, `codex`, (unknown member) → `null`.
 */
export function memberShortLabel(member: TFleetMember | undefined): string | null {
  if (!member) return null;
  const agent = member.agent_id || '';
  const model = member.model?.trim();
  if (!agent && !model) return null;
  if (agent && model) return `${agent} · ${model}`;
  return agent || model || null;
}

/** Resolve the agent logo for a fleet member from its `agent_id` slug. */
export function memberLogo(member: TFleetMember | undefined): string | null {
  if (!member?.agent_id) return null;
  return resolveAgentLogo({ backend: member.agent_id });
}
