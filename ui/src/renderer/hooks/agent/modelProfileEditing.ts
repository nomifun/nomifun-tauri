import type { ModelProfile, ModelTask, ModelTrait } from '@/common/config/storage';
import type { ModelProfileUpsertRequest } from '@/common/types/provider/providerApi';

/** Display order of modality/task options in model profile editors. */
export const MODEL_TASK_ORDER: ModelTask[] = [
  'chat',
  'image_generation',
  'image_edit',
  'video_generation',
  'speech_synthesis',
  'speech_recognition',
  'embedding',
  'rerank',
];

export const editableModelTasks = (profile?: ModelProfile): ModelTask[] => {
  if (profile?.source !== 'user') return [];
  return profile.tasks ?? [];
};

export const editableModelTraits = (profile?: ModelProfile): ModelTrait[] => {
  if (profile?.source !== 'user') return [];
  return profile.traits ?? [];
};

export const visibleModelTaskBadges = (profile?: ModelProfile): ModelTask[] =>
  editableModelTasks(profile).filter((task) => task !== 'chat');

export const buildModelProfileUpsertRequest = (
  providerId: string,
  model: string,
  tasks: ModelTask[],
  traits: ModelTrait[]
): ModelProfileUpsertRequest => ({
  provider_id: providerId,
  model,
  tasks,
  traits,
  source: 'user',
});
