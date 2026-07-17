import { NOMIFUN_FREE_MODEL_PLATFORM } from '@/common/types/provider/managedModelService';
import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';

type ModelSelectorProvider = {
  name?: string;
  platform?: string;
};

type ManagedModelProviderLabels = {
  free: string;
};

export const formatModelSelectorProviderLabel = (
  provider: ModelSelectorProvider,
  labels: ManagedModelProviderLabels,
): string => {
  if (provider.platform === NOMIFUN_FREE_MODEL_PLATFORM) return labels.free;
  return provider.name?.trim() || provider.platform?.trim() || '';
};

/** Localized provider label for every model-selection surface. */
export const useModelSelectorProviderLabel = () => {
  const { t } = useTranslation();
  const free = t('settings.modelHub.free.title');

  return useCallback(
    (provider: ModelSelectorProvider) => formatModelSelectorProviderLabel(provider, { free }),
    [free],
  );
};
