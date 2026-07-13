/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Creative Workshop model discovery.
 *
 * Answers "which providers/models can generate images / videos?" for the
 * Model Hub 创作模型 view AND for the workshop generation card (M7). The signal
 * is a NAME heuristic (`hasSpecificModelCapability`, twin of the backend
 * `nomifun_api_types::infer_generation_capabilities`), layered with an optional
 * provider-level user override via the existing `capabilities` +
 * `is_user_selected` mechanism — no schema change, computed entirely in this
 * read layer.
 *
 * M7 usage: read providers via `useProvidersQuery()`, then call
 * `getCreationModels(providers, 'image_generation' | 'video_generation')`.
 * Each entry exposes `{ providerId, model, capabilities }` — feed `providerId`
 * + `model` straight into a `POST /api/creation/tasks` body.
 */

import type { IProvider, ModelProfile, ModelTask } from '@/common/config/storage';
import { hasSpecificModelCapability } from '@/common/utils/modelCapabilities';

/** The two Creative-Workshop generation capabilities. */
export type CreationCapability = 'image_generation' | 'video_generation';

export const CREATION_CAPABILITIES: CreationCapability[] = ['image_generation', 'video_generation'];

/** One generation-capable model resolved against a provider. */
export interface CreationModelEntry {
  providerId: string;
  providerName: string;
  platform: string;
  model: string;
  /** Non-empty subset of {@link CreationCapability}. */
  capabilities: CreationCapability[];
}

/** Generation-capable models grouped under their provider. */
export interface CreationProviderGroup {
  providerId: string;
  providerName: string;
  platform: string;
  models: CreationModelEntry[];
}

/**
 * Provider-level user override for a capability, read from `capabilities` +
 * `is_user_selected`:
 * - `true`  → user explicitly marked this platform as capable (escape hatch for
 *   custom / self-hosted providers whose model names miss the heuristic).
 * - `false` → user explicitly disabled it for the whole platform.
 * - `undefined` → no override; fall back to the name heuristic.
 */
export const providerCapabilityOverride = (
  provider: IProvider,
  cap: CreationCapability
): boolean | undefined => provider.capabilities?.find((c) => c.type === cap)?.is_user_selected;

/** Whether a model is enabled (defaults to enabled when unset). */
const isModelEnabled = (provider: IProvider, model: string): boolean =>
  provider.model_enabled?.[model] !== false;

/**
 * Map an authoritative {@link ModelProfile} task to the Creative-Workshop
 * capability it satisfies. `image_edit` is surfaced under `image_generation`:
 * an edit-capable model can also produce images, so the image picker offers it.
 * (The workshop read layer has no standalone `image_edit` capability — see
 * {@link CreationCapability}.)
 */
const TASK_TO_CREATION_CAP: Partial<Record<ModelTask, CreationCapability>> = {
  image_generation: 'image_generation',
  image_edit: 'image_generation',
  video_generation: 'video_generation',
};

/** Creation capabilities implied by a model profile's declared tasks. */
const profileCreationCapabilities = (profile: ModelProfile): CreationCapability[] => {
  const caps = new Set<CreationCapability>();
  for (const task of profile.tasks ?? []) {
    const cap = TASK_TO_CREATION_CAP[task];
    if (cap) caps.add(cap);
  }
  return [...caps];
};

type ProfileCapabilityIndex = Map<string, CreationCapability[]>;

/** Composite key for the per-model profile lookup (collision-free via JSON). */
const profileKey = (providerId: string, model: string): string => JSON.stringify([providerId, model]);

/**
 * Index authoritative per-model profiles by `(providerId, model)`. User edits
 * and NomiFun's pinned local catalog are authoritative; name-inferred rows are
 * skipped so automatic guesses retain the legacy heuristic behavior.
 */
const buildAuthoritativeProfileIndex = (
  profiles: ModelProfile[] | undefined
): ProfileCapabilityIndex | undefined => {
  if (!profiles || profiles.length === 0) return undefined;
  const index: ProfileCapabilityIndex = new Map();
  for (const profile of profiles) {
    if (profile.source === 'inferred') continue;
    index.set(profileKey(profile.provider_id, profile.model), profileCreationCapabilities(profile));
  }
  return index.size > 0 ? index : undefined;
};

/**
 * Resolve whether a specific model has a creation capability. Precedence:
 *   1. authoritative user/catalog profile (`profileCaps`) — sole authority
 *      when present, both positively and negatively;
 *   2. provider-level user override (`capabilities` + `is_user_selected`);
 *   3. the model-name heuristic.
 */
export const modelHasCreationCapability = (
  provider: IProvider,
  model: string,
  cap: CreationCapability,
  profileCaps?: CreationCapability[]
): boolean => {
  if (profileCaps !== undefined) return profileCaps.includes(cap);
  const override = providerCapabilityOverride(provider, cap);
  if (override !== undefined) return override;
  return hasSpecificModelCapability(provider, model, cap) === true;
};

/** All creation capabilities a model resolves to (possibly empty). */
export const resolveModelCreationCapabilities = (
  provider: IProvider,
  model: string,
  profileCaps?: CreationCapability[]
): CreationCapability[] =>
  CREATION_CAPABILITIES.filter((cap) => modelHasCreationCapability(provider, model, cap, profileCaps));

/**
 * Flat list of generation-capable models across enabled providers.
 *
 * @param providers raw provider list (from `useProvidersQuery()`)
 * @param filter    optionally restrict to a single capability
 * @param profiles  authoritative per-model profiles (from `useModelProfiles()`);
 *                  user-set entries override the name heuristic per model
 */
export const getCreationModels = (
  providers: IProvider[] | undefined,
  filter?: CreationCapability,
  profiles?: ModelProfile[]
): CreationModelEntry[] => {
  const profileIndex = buildAuthoritativeProfileIndex(profiles);
  const out: CreationModelEntry[] = [];
  for (const provider of providers ?? []) {
    if (provider.enabled === false) continue;
    for (const model of provider.models ?? []) {
      if (!isModelEnabled(provider, model)) continue;
      const profileCaps = profileIndex?.get(profileKey(provider.id, model));
      const capabilities = resolveModelCreationCapabilities(provider, model, profileCaps);
      if (capabilities.length === 0) continue;
      if (filter && !capabilities.includes(filter)) continue;
      out.push({
        providerId: provider.id,
        providerName: provider.name,
        platform: provider.platform,
        model,
        capabilities,
      });
    }
  }
  return out;
};

/** Group the flat entry list by provider, preserving provider order. */
export const groupCreationModelsByProvider = (entries: CreationModelEntry[]): CreationProviderGroup[] => {
  const groups = new Map<string, CreationProviderGroup>();
  for (const entry of entries) {
    let group = groups.get(entry.providerId);
    if (!group) {
      group = {
        providerId: entry.providerId,
        providerName: entry.providerName,
        platform: entry.platform,
        models: [],
      };
      groups.set(entry.providerId, group);
    }
    group.models.push(entry);
  }
  return [...groups.values()];
};

/** Count of generation-capable models for a capability (for filter badges). */
export const countCreationModels = (
  providers: IProvider[] | undefined,
  filter?: CreationCapability,
  profiles?: ModelProfile[]
): number => getCreationModels(providers, filter, profiles).length;
