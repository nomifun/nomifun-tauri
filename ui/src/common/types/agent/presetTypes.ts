/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import {
  parsePresetId,
  type AgentId,
  type KnowledgeBaseId,
  type PresetId,
  type PresetTagId,
  type ProviderId,
} from '../ids';

declare const presetTagKeyBrand: unique symbol;

/** Every product preset, including materialized builtin/extension presets, has
 * one durable bare UUIDv7 business identity. Catalog lineage is `source_key`.
 */
export type PresetReference = PresetId;
/** Unified natural key for builtin and user-created preset tags. */
export type PresetTagKey = string & { readonly [presetTagKeyBrand]: true };

const nonEmptyNaturalKey = <T extends string>(value: unknown, label: string): T => {
  if (typeof value !== 'string' || value.trim() !== value || value.length === 0) {
    throw new TypeError(`${label} must be a non-empty canonical natural key`);
  }
  return value as T;
};

export const parsePresetReference = (
  value: unknown,
  _source?: PresetSource,
): PresetReference => {
  return parsePresetId(value);
};

/**
 * Snapshots retain the same durable preset business ID as the live product
 * object. There is no source-dependent wire union.
 */
export const parsePresetSnapshotReference = (value: unknown): PresetReference => {
  return parsePresetId(value);
};

const PRESET_TAG_KEY = /^[a-z0-9_.:-]+$/;

export const parsePresetTagKey = (value: unknown): PresetTagKey => {
  const key = nonEmptyNaturalKey<PresetTagKey>(value, 'preset tag key');
  if (key.length > 255 || !PRESET_TAG_KEY.test(key)) {
    throw new TypeError('preset tag key must contain only lowercase ASCII letters, digits, _, -, ., or :');
  }
  return key;
};

// Mirror of nomifun-api-types/src/preset.rs.
// Any shape change on either side requires a same-PR update on the other.

export type PresetSource = 'builtin' | 'user' | 'extension';
export type PresetTarget = 'conversation' | 'execution_step' | 'companion' | 'public_companion' | 'cron';

export interface AgentPreference {
  agent_id: AgentId;
  required: boolean;
}

export interface ModelPreference {
  provider_id?: ProviderId;
  model: string;
  required: boolean;
}

export interface SkillBinding {
  skill_name: string;
  required: boolean;
}

export interface KnowledgeBaseBinding {
  knowledge_base_id: KnowledgeBaseId;
  required: boolean;
}

export interface PresetKnowledgePolicy {
  enabled: boolean;
  mode: string;
  writeback: boolean;
  eagerness?: 'conservative' | 'aggressive';
  grounded: boolean;
}

export interface Preset {
  /**
   * All presets use bare canonical UUIDv7 IDs. Builtin/extension catalog
   * lineage stays in `source_key`; generic `id` is not part of the contract.
   */
  preset_id: PresetReference;
  revision: number;
  source: PresetSource;
  source_key?: string;
  name: string;
  name_i18n: Record<string, string>;
  description?: string;
  description_i18n: Record<string, string>;
  routing_description?: string;
  instructions: string;
  instructions_i18n: Record<string, string>;
  avatar?: string;
  fallback_allowed: boolean;
  preferred_agent_id?: AgentId;
  targets: PresetTarget[];
  agent_preferences: AgentPreference[];
  model_preferences: ModelPreference[];
  included_skills: SkillBinding[];
  excluded_auto_skills: string[];
  knowledge_policy: PresetKnowledgePolicy;
  knowledge_bases: KnowledgeBaseBinding[];
  examples: string[];
  examples_i18n: Record<string, string[]>;
  audience_tag_ids: PresetTagId[];
  scenario_tag_ids: PresetTagId[];
  enabled: boolean;
  auto_selectable: boolean;
  sort_order: number;
  last_used_at?: number;
}

export interface CreatePresetRequest {
  preset_id?: PresetId;
  name: string;
  description?: string;
  routing_description?: string;
  instructions?: string;
  avatar?: string;
  fallback_allowed?: boolean;
  targets?: PresetTarget[];
  agent_preferences?: AgentPreference[];
  model_preferences?: ModelPreference[];
  included_skills?: SkillBinding[];
  excluded_auto_skills?: string[];
  knowledge_policy?: PresetKnowledgePolicy;
  knowledge_bases?: KnowledgeBaseBinding[];
  examples?: string[];
  examples_i18n?: Record<string, string[]>;
  audience_tag_ids?: PresetTagId[];
  scenario_tag_ids?: PresetTagId[];
  name_i18n?: Record<string, string>;
  description_i18n?: Record<string, string>;
  instructions_i18n?: Record<string, string>;
}

export type UpdatePresetRequest = Partial<Omit<CreatePresetRequest, 'preset_id'>>;

export interface SetPresetStateRequest {
  preset_id: PresetReference;
  enabled?: boolean;
  auto_selectable?: boolean;
  sort_order?: number;
  last_used_at?: number;
  /** Empty string clears the per-user preference. */
  preferred_agent_id?: AgentId;
}

export interface PresetOverrides {
  agent_id?: AgentId;
  provider_id?: ProviderId;
  model?: string;
  instructions?: string;
  include_skills?: string[];
  exclude_skills?: string[];
  knowledge_policy?: PresetKnowledgePolicy;
  knowledge_base_ids?: KnowledgeBaseId[];
}

export interface ResolvePresetRequest {
  preset_id: PresetReference;
  target: PresetTarget;
  locale?: string;
  overrides?: PresetOverrides;
}

export interface ResolvedPresetSnapshot {
  preset_id: PresetReference;
  preset_revision: number;
  preset_name: string;
  target: PresetTarget;
  routing_description?: string;
  instructions: string;
  resolved_agent_id?: AgentId;
  resolved_agent_type?: string;
  resolved_agent_backend?: string;
  resolved_model?: ModelPreference;
  included_skills: string[];
  excluded_auto_skills: string[];
  knowledge_policy: PresetKnowledgePolicy;
  knowledge_base_ids: KnowledgeBaseId[];
  warnings: string[];
}

export interface ImportPresetsRequest {
  presets: CreatePresetRequest[];
}

export interface PresetImportError {
  preset_id: string;
  error: string;
}

export interface ImportPresetsResult {
  imported: number;
  skipped: number;
  failed: number;
  errors: PresetImportError[];
}

export type PresetTagDimension = 'audience' | 'scenario';

export interface PresetTag {
  preset_tag_id: PresetTagId;
  key: PresetTagKey;
  dimension: PresetTagDimension;
  label: string;
  label_i18n: Record<string, string>;
  sort_order: number;
  builtin: boolean;
}

export interface CreatePresetTagRequest {
  dimension: PresetTagDimension;
  label: string;
}

export interface UpdatePresetTagRequest {
  preset_tag_id: PresetTagId;
  label?: string;
  sort_order?: number;
}
