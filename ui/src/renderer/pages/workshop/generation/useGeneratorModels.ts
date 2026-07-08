/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Resolve the models a generation card can pick, by mode:
 *  - image → providers' `image_generation`-capable models (M6 heuristic + override)
 *  - video → providers' `video_generation`-capable models
 *  - text  → providers' conversation models (the Model Hub "available" set, which
 *            already excludes image/video generators via `excludeFromPrimary`)
 *
 * Grouped by provider and flattened, plus a `hasProviders` signal so the picker
 * can tell "no platforms configured" apart from "no matching models".
 */

import { useMemo } from 'react';
import { useProvidersQuery, useModelProviderList } from '@renderer/hooks/agent/useModelProviderList';
import { getCreationModels } from '@renderer/pages/modelHub/creationModels';
import type { GenMode, ModelGroup, ModelOption } from './genTypes';

export interface GeneratorModels {
  groups: ModelGroup[];
  flat: ModelOption[];
  /** Any enabled provider exposes at least one usable model at all. */
  hasProviders: boolean;
}

function group(flat: ModelOption[]): ModelGroup[] {
  const groups = new Map<string, ModelGroup>();
  for (const m of flat) {
    let g = groups.get(m.providerId);
    if (!g) {
      g = { providerId: m.providerId, providerName: m.providerName, platform: m.platform, models: [] };
      groups.set(m.providerId, g);
    }
    g.models.push(m);
  }
  return [...groups.values()];
}

export function useGeneratorModels(mode: GenMode): GeneratorModels {
  const { data: rawProviders } = useProvidersQuery();
  const { providers: convProviders, getAvailableModels } = useModelProviderList();

  return useMemo<GeneratorModels>(() => {
    const hasProviders =
      (rawProviders ?? []).some((p) => p.enabled !== false && (p.models ?? []).length > 0) || convProviders.length > 0;

    if (mode === 'text') {
      const flat: ModelOption[] = [];
      for (const p of convProviders) {
        for (const model of getAvailableModels(p)) {
          flat.push({ providerId: p.id, providerName: p.name, platform: p.platform, model });
        }
      }
      return { groups: group(flat), flat, hasProviders };
    }

    const cap = mode === 'video' ? 'video_generation' : 'image_generation';
    const flat: ModelOption[] = getCreationModels(rawProviders, cap).map((e) => ({
      providerId: e.providerId,
      providerName: e.providerName,
      platform: e.platform,
      model: e.model,
    }));
    return { groups: group(flat), flat, hasProviders };
  }, [mode, rawProviders, convProviders, getAvailableModels]);
}
