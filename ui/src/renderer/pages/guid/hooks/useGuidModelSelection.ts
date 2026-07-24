/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IProvider, TProviderWithModel } from '@/common/config/storage';
import type { ConfigKeyMap } from '@/common/config/configKeys';
import { configService } from '@/common/config/configService';
import { useGoogleAuthModels } from '@/renderer/hooks/agent/useGoogleAuthModels';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';
import { orderModelSelectorProviders } from '@/renderer/hooks/agent/modelSelectorProviderOrdering';
import { getAvailableModels, hasAvailableModels } from '../utils/modelUtils';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';

/**
 * Build a unique key for a provider/model pair.
 */
const buildModelKey = (providerId?: string, modelName?: string) => {
  if (!providerId || !modelName) return null;
  return `${providerId}:${modelName}`;
};

/**
 * Check if a model key still exists in the provider list.
 */
const isModelKeyAvailable = (key: string | null, providers?: IProvider[]) => {
  if (!key || !providers || providers.length === 0) return false;
  return providers.some((provider) => {
    if (!provider.id || !provider.models?.length) return false;
    return provider.models.some((modelName) => buildModelKey(provider.id, modelName) === key);
  });
};

/** Provider-based agent keys that share the model list UI */
type ProviderAgentKey = 'nomi';

/** Map agent key → storage key for persisting default model */
const MODEL_STORAGE_KEY: Record<ProviderAgentKey, 'nomi.defaultModel'> = {
  nomi: 'nomi.defaultModel',
};

type PersistedDefaultModel = NonNullable<ConfigKeyMap['nomi.defaultModel']>;

function isPersistedDefaultModel(value: unknown): value is PersistedDefaultModel {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    !('id' in object) &&
    typeof object.provider_id === 'string' &&
    typeof object.model === 'string'
  );
}

export type GuidModelSelectionResult = {
  modelList: IProvider[];
  isGoogleAuth: boolean;
  formatGeminiModelLabel: (provider: { platform?: string } | undefined, modelName?: string) => string;
  current_model: TProviderWithModel | undefined;
  setCurrentModel: (model_info: TProviderWithModel) => Promise<void>;
};

/**
 * Hook that manages the model list and selection state for the Guid page.
 * @param agentKey - current provider-based agent (currently only 'nomi')
 */
export const useGuidModelSelection = (agentKey: ProviderAgentKey = 'nomi'): GuidModelSelectionResult => {
  const { isGoogleAuth } = useGoogleAuthModels();
  const { data: modelConfig } = useProvidersQuery();

  const modelList = useMemo(() => {
    const allProviders: IProvider[] = (modelConfig || []).filter((platform) => !!platform.models.length);
    return orderModelSelectorProviders(allProviders.filter(hasAvailableModels));
  }, [modelConfig]);

  const formatGeminiModelLabel = useCallback((_provider: { platform?: string } | undefined, modelName?: string) => {
    if (!modelName) return '';
    return modelName;
  }, []);

  const [current_model, _setCurrentModel] = useState<TProviderWithModel>();
  const selectedModelKeyRef = useRef<string | null>(null);
  const prevStorageKeyRef = useRef<string | null>(null);

  const storageKey = MODEL_STORAGE_KEY[agentKey];

  const setCurrentModel = useCallback(
    async (model_info: TProviderWithModel, persist = true) => {
      selectedModelKeyRef.current = buildModelKey(model_info.id, model_info.use_model);
      if (persist) {
        await configService.set(storageKey, {
          provider_id: model_info.id,
          model: model_info.use_model,
        }).catch((error) => {
          console.error('Failed to save default model:', error);
        });
      }
      _setCurrentModel(model_info);
    },
    [storageKey]
  );

  // Set default model when modelList or agent changes
  useEffect(() => {
    const setDefaultModel = async () => {
      if (!modelList || modelList.length === 0) {
        return;
      }
      // When agent switches, reset selection so we reload from the new storage key
      const agentChanged = prevStorageKeyRef.current !== null && prevStorageKeyRef.current !== storageKey;
      prevStorageKeyRef.current = storageKey;
      if (agentChanged) {
        selectedModelKeyRef.current = null;
      }

      const currentKey = selectedModelKeyRef.current || buildModelKey(current_model?.id, current_model?.use_model);
      if (!agentChanged && isModelKeyAvailable(currentKey, modelList)) {
        if (!selectedModelKeyRef.current && currentKey) {
          selectedModelKeyRef.current = currentKey;
        }
        return;
      }
      const rawSavedModel: unknown = configService.get(storageKey);
      const savedModel = isPersistedDefaultModel(rawSavedModel) ? rawSavedModel : undefined;
      const canPersistFallback = rawSavedModel === undefined || savedModel !== undefined;
      if (rawSavedModel !== undefined && savedModel === undefined) {
        console.warn(`Ignoring invalid persisted default model for ${storageKey}; no legacy migration is performed.`);
      }

      // First-available enabled model — the fallback whenever nothing valid was
      // saved. `modelList` is already filtered by `hasAvailableModels`, so the
      // first provider is guaranteed to expose at least one selectable model.
      // Use `getAvailableModels(provider)[0]` (the FILTERED list the picker shows)
      // rather than raw `provider.models[0]`, which can be a model that lacks
      // function_calling / is excludeFromPrimary and thus never appears in the
      // picker — picking it would leave current_model pointing at an unselectable
      // model. This guarantees the lead (主管) model is always set and editable,
      // so submit is never silently blocked in auto/range mode.
      const firstProvider = modelList[0];
      const firstAvailableModel = firstProvider ? (getAvailableModels(firstProvider)[0] ?? '') : '';

      let defaultModel: IProvider | undefined;
      let resolvedUseModel: string;

      if (savedModel) {
        const { provider_id, model } = savedModel;
        const exactMatch = modelList.find((m) => m.id === provider_id);
        if (exactMatch && getAvailableModels(exactMatch).includes(model)) {
          defaultModel = exactMatch;
          resolvedUseModel = model;
        } else {
          defaultModel = firstProvider;
          resolvedUseModel = firstAvailableModel;
        }
      } else {
        defaultModel = firstProvider;
        resolvedUseModel = firstAvailableModel;
      }

      if (!defaultModel || !resolvedUseModel) return;

      await setCurrentModel({
        ...defaultModel,
        use_model: resolvedUseModel,
      }, canPersistFallback);
    };

    setDefaultModel().catch((error) => {
      console.error('Failed to set default model:', error);
    });
  }, [modelList, storageKey]);

  return {
    modelList,
    isGoogleAuth,
    formatGeminiModelLabel,
    current_model,
    setCurrentModel,
  };
};
