/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TFleetMember, TFleetMemberInput } from '@/common/types/orchestrator/orchestratorTypes';

/**
 * Shared draft shape + curated vocabularies for the fleet editor. The drawer
 * edits members as flat `FleetMemberDraft` rows (the nested wire shape — a
 * `capability_profile` object + a `constraints` object — is awkward to bind to
 * form controls), then `toMemberInput` reassembles the wire payload on save and
 * `fromMember` flattens a persisted member back into a draft for editing.
 */

/** Flat, form-friendly member shape edited inside the drawer. */
export type FleetMemberDraft = {
  /** Stable client key for React list rendering (NOT persisted). */
  key: string;
  agent_id: string;
  provider_id?: string;
  model?: string;
  role_hint?: string;
  strengths: string[];
  max_concurrency?: number;
  cost_tier?: string;
};

/** Curated strength tags (i18n keys under `orchestrator.fleet.strength.*`). */
export const STRENGTH_KEYS = [
  'coding',
  'reasoning',
  'planning',
  'research',
  'writing',
  'review',
  'longContext',
  'speed',
  'vision',
  'tools',
] as const;

/** Cost tiers (i18n keys under `orchestrator.fleet.costTier.*`). */
export const COST_TIER_KEYS = ['low', 'medium', 'high'] as const;

let draftKeySeq = 0;
/** Monotonic client-only key for a fresh draft row. */
export const nextDraftKey = (): string => `member-${Date.now()}-${draftKeySeq++}`;

/** A blank member draft (used when adding a row). */
export const blankMemberDraft = (): FleetMemberDraft => ({
  key: nextDraftKey(),
  agent_id: '',
  strengths: [],
});

/** Flatten a persisted member into an editable draft. */
export const fromMember = (member: TFleetMember): FleetMemberDraft => ({
  key: member.id || nextDraftKey(),
  agent_id: member.agent_id,
  provider_id: member.provider_id,
  model: member.model,
  role_hint: member.role_hint,
  strengths: member.capability_profile?.strengths ?? [],
  max_concurrency: member.constraints?.max_concurrency,
  cost_tier: member.constraints?.cost_tier,
});

/** Reassemble a draft row into the wire input payload. */
export const toMemberInput = (draft: FleetMemberDraft, sort_order: number): TFleetMemberInput => {
  const hasProfile = draft.strengths.length > 0 || Boolean(draft.cost_tier);
  const hasConstraints = typeof draft.max_concurrency === 'number' || Boolean(draft.cost_tier);

  return {
    agent_id: draft.agent_id,
    provider_id: draft.provider_id || undefined,
    model: draft.model || undefined,
    role_hint: draft.role_hint || undefined,
    sort_order,
    capability_profile: hasProfile
      ? {
          strengths: draft.strengths,
          modalities: [],
          tools: false,
          reasoning: '',
          cost_tier: draft.cost_tier ?? '',
          speed_tier: '',
        }
      : undefined,
    constraints: hasConstraints
      ? {
          max_concurrency: draft.max_concurrency,
          cost_tier: draft.cost_tier || undefined,
        }
      : undefined,
  };
};
