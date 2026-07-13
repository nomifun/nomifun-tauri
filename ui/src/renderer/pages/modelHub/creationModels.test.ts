/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IProvider, ModelProfile } from '@/common/config/storage';
import { getCreationModels } from './creationModels';

const provider = {
  id: 'nomifun-local-model',
  name: 'Local Models',
  platform: 'nomifun-local-model',
  enabled: true,
  models: ['z-image-turbo-q3-k', 'stable-diffusion-chat-lookalike'],
  model_enabled: {},
} as unknown as IProvider;

const profile = (model: string, tasks: ModelProfile['tasks']): ModelProfile => ({
  provider_id: provider.id,
  model,
  tasks,
  traits: [],
  params: {},
  source: 'catalog',
  updated_at: 1,
});

describe('creation model catalog authority', () => {
  test('catalog profiles expose local image models and override name guesses', () => {
    const result = getCreationModels(
      [provider],
      'image_generation',
      [
        profile('z-image-turbo-q3-k', ['image_generation']),
        profile('stable-diffusion-chat-lookalike', ['chat']),
      ]
    );

    expect(result.map((entry) => entry.model)).toEqual(['z-image-turbo-q3-k']);
    expect(result[0].capabilities).toEqual(['image_generation']);
  });
});
