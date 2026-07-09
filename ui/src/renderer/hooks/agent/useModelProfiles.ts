import useSWR, { type SWRConfiguration } from 'swr';

import { ipcBridge } from '@/common';
import type { ModelProfile } from '@/common/config/storage';

export const MODEL_PROFILES_SWR_KEY = 'model-profiles';

const SWR_OPTIONS: SWRConfiguration<ModelProfile[], Error> = {
  revalidateOnFocus: false,
  revalidateOnReconnect: false,
  shouldRetryOnError: false,
};

export const fetchModelProfiles = async (): Promise<ModelProfile[]> => {
  return (await ipcBridge.modelProfile.list.invoke()) ?? [];
};

/**
 * Authoritative per-model capability profiles (multimodal model hub).
 * Returns the list plus a `profileFor(providerId, model)` lookup and mutate.
 */
export const useModelProfiles = () => {
  const { data, error, isLoading, mutate } = useSWR<ModelProfile[]>(
    MODEL_PROFILES_SWR_KEY,
    fetchModelProfiles,
    SWR_OPTIONS
  );

  const profiles = data ?? [];
  const profileFor = (providerId: string, model: string): ModelProfile | undefined =>
    profiles.find((p) => p.provider_id === providerId && p.model === model);

  return { profiles, profileFor, error, isLoading, mutate };
};
