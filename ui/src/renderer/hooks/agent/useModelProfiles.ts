import useSWR, { type SWRConfiguration } from 'swr';

import { ipcBridge } from '@/common';
import type { ModelProfile, ModelTask } from '@/common/config/storage';

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

/**
 * Frontend twin of the backend `derive_tasks_and_traits` seed (model_task.rs).
 * Used to pre-fill the modality selector when entering a new model. Platform is
 * authoritative where unambiguous (StepFun Step Plan = image-only product).
 */
export const deriveDefaultTasks = (platform: string, model: string): ModelTask[] => {
  const p = platform.toLowerCase();
  const base = model.toLowerCase();
  const tasks: ModelTask[] = [];
  const push = (t: ModelTask) => {
    if (!tasks.includes(t)) tasks.push(t);
  };

  if (p === 'stepfun-plan') {
    push('image_generation');
    push('image_edit');
  }
  const IMAGE = ['gpt-image', 'dall-e', 'dall', 'seedream', 'flux', 'stable-diffusion', 'sd-', 'sdxl', 'imagen', 'midjourney', 'kolors', 'hidream', 'janus', 'cogview', 'diffusion', 'image'];
  const VIDEO = ['sora', 'veo', 'kling', 'seedance', 'wanx', 'wan2', 'hailuo', 'vidu', 'cogvideo', 'pixverse', 'runway', 'luma', 'dream-machine'];
  if (IMAGE.some((k) => base.includes(k))) push('image_generation');
  if (VIDEO.some((k) => base.includes(k))) push('video_generation');
  if (tasks.includes('image_generation') && (base.includes('edit') || base.includes('inpaint'))) push('image_edit');

  if (['rerank'].some((k) => base.includes(k))) push('rerank');
  else if (['embed', 'bge-', 'gte-', '-e5-'].some((k) => base.includes(k))) push('embedding');
  else if (['whisper', 'asr', 'transcrib', 'sensevoice', 'paraformer', 'nova-2', 'nova-3'].some((k) => base.includes(k))) push('speech_recognition');
  else if (['tts', 'text-to-speech', 'cosyvoice', '-voice'].some((k) => base.includes(k))) push('speech_synthesis');

  if (tasks.length === 0) push('chat');
  return tasks;
};
