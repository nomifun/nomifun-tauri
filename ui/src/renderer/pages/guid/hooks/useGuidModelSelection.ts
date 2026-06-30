/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import type { IProvider, TProviderWithModel } from '@/common/config/storage';
import { configService } from '@/common/config/configService';
import { useGoogleAuthModels } from '@/renderer/hooks/agent/useGoogleAuthModels';
import { useProvidersQuery } from '@/renderer/hooks/agent/useModelProviderList';
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
    return allProviders.filter(hasAvailableModels);
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
    async (model_info: TProviderWithModel) => {
      selectedModelKeyRef.current = buildModelKey(model_info.id, model_info.use_model);
      await configService.set(storageKey, { id: model_info.id, use_model: model_info.use_model }).catch((error) => {
        console.error('Failed to save default model:', error);
      });
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
      const savedModel = configService.get(storageKey);

      const isNewFormat = savedModel && typeof savedModel === 'object' && 'id' in savedModel;

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

      if (isNewFormat) {
        const { id, use_model } = savedModel;
        const exactMatch = modelList.find((m) => m.id === id);
        if (exactMatch && getAvailableModels(exactMatch).includes(use_model)) {
          defaultModel = exactMatch;
          resolvedUseModel = use_model;
        } else {
          defaultModel = firstProvider;
          resolvedUseModel = firstAvailableModel;
        }
      } else if (typeof savedModel === 'string') {
        defaultModel = modelList.find((m) => getAvailableModels(m).includes(savedModel)) || firstProvider;
        resolvedUseModel = defaultModel && getAvailableModels(defaultModel).includes(savedModel) ? savedModel : firstAvailableModel;
      } else {
        defaultModel = firstProvider;
        resolvedUseModel = firstAvailableModel;
      }

      if (!defaultModel || !resolvedUseModel) return;

      await setCurrentModel({
        ...defaultModel,
        use_model: resolvedUseModel,
      });
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
