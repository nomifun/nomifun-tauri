/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IProvider, ModelProfile } from '@/common/config/storage';
import { getCreationModels } from './creationModels';

const provider = {
  id: 'prov_0190f5fe-7c00-7a00-8000-000000000001',
  name: 'Example Provider',
  platform: 'openai',
  enabled: true,
  models: ['custom-visual-v1', 'stable-diffusion-chat-lookalike'],
  model_enabled: {},
} as unknown as IProvider;

const profile = (model: string, tasks: ModelProfile['tasks']): ModelProfile => ({
  provider_id: provider.id,
  model,
  tasks,
  traits: [],
  params: {},
  source: 'user',
  updated_at: 1,
});

describe('creation model profile authority', () => {
  test('user profiles expose image models and override name guesses', () => {
    const result = getCreationModels(
      [provider],
      'image_generation',
      [
        profile('custom-visual-v1', ['image_generation']),
        profile('stable-diffusion-chat-lookalike', ['chat']),
      ]
    );

    expect(result.map((entry) => entry.model)).toEqual(['custom-visual-v1']);
    expect(result[0].capabilities).toEqual(['image_generation']);
  });
});
