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

import type { IProvider } from '@/common/config/storage';
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
 * Resolve whether a specific model has a creation capability: provider-level
 * user override wins, otherwise the name heuristic.
 */
export const modelHasCreationCapability = (
  provider: IProvider,
  model: string,
  cap: CreationCapability
): boolean => {
  const override = providerCapabilityOverride(provider, cap);
  if (override !== undefined) return override;
  return hasSpecificModelCapability(provider, model, cap) === true;
};

/** All creation capabilities a model resolves to (possibly empty). */
export const resolveModelCreationCapabilities = (
  provider: IProvider,
  model: string
): CreationCapability[] => CREATION_CAPABILITIES.filter((cap) => modelHasCreationCapability(provider, model, cap));

/**
 * Flat list of generation-capable models across enabled providers.
 *
 * @param providers raw provider list (from `useProvidersQuery()`)
 * @param filter    optionally restrict to a single capability
 */
export const getCreationModels = (
  providers: IProvider[] | undefined,
  filter?: CreationCapability
): CreationModelEntry[] => {
  const out: CreationModelEntry[] = [];
  for (const provider of providers ?? []) {
    if (provider.enabled === false) continue;
    for (const model of provider.models ?? []) {
      if (!isModelEnabled(provider, model)) continue;
      const capabilities = resolveModelCreationCapabilities(provider, model);
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
  filter?: CreationCapability
): number => getCreationModels(providers, filter).length;
