import { NOMIFUN_FREE_MODEL_PLATFORM } from '@/common/types/provider/managedModelService';

const modelSelectorProviderRank = (platform?: string): number => {
  if (platform === NOMIFUN_FREE_MODEL_PLATFORM) return 1;
  return 0;
};

/**
 * Keep supplier providers easiest to reach in model selectors, followed by the
 * managed free provider last. Relative
 * priority inside each group remains exactly as returned by the provider API.
 */
export const orderModelSelectorProviders = <T extends { platform?: string }>(providers: readonly T[]): T[] =>
  providers
    .map((provider, index) => ({ provider, index }))
    .sort((left, right) => {
      const rankDifference =
        modelSelectorProviderRank(left.provider.platform) - modelSelectorProviderRank(right.provider.platform);
      return rankDifference || left.index - right.index;
    })
    .map(({ provider }) => provider);
