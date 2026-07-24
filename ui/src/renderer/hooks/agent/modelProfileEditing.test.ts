import { describe, expect, test } from 'bun:test';

import type { ModelProfile } from '@/common/config/storage';
import { parseProviderId } from '@/common/types/ids';
import {
  buildModelProfileUpsertRequest,
  editableModelTasks,
  editableModelTraits,
  visibleModelTaskBadges,
} from './modelProfileEditing';

const providerId = parseProviderId('0190f5fe-7c00-7a00-8000-000000000001');

const profile = (source: ModelProfile['source'], tasks: ModelProfile['tasks'], traits: ModelProfile['traits'] = []): ModelProfile => ({
  provider_id: providerId,
  model: 'happyhorse-1.0',
  tasks,
  traits,
  params: {},
  source,
  updated_at: 1,
});

describe('model profile editing helpers', () => {
  test('treats inferred profiles as empty user-editable categories', () => {
    const inferred = profile('inferred', ['video_generation']);

    expect(editableModelTasks(inferred)).toEqual([]);
    expect(editableModelTraits(inferred)).toEqual([]);
    expect(visibleModelTaskBadges(inferred)).toEqual([]);
  });

  test('uses user-selected tasks as the only visible category badges', () => {
    const user = profile('user', ['chat', 'image_generation', 'video_generation'], ['vision_input']);

    expect(editableModelTasks(user)).toEqual(['chat', 'image_generation', 'video_generation']);
    expect(editableModelTraits(user)).toEqual(['vision_input']);
    expect(visibleModelTaskBadges(user)).toEqual(['image_generation', 'video_generation']);
  });

  test('persists an empty user profile instead of falling back to a default task', () => {
    expect(buildModelProfileUpsertRequest(providerId, 'happyhorse-1.0', [], [])).toEqual({
      provider_id: providerId,
      model: 'happyhorse-1.0',
      tasks: [],
      traits: [],
      source: 'user',
    });
  });
});
