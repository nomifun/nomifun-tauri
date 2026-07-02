/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IProvider } from '@/common/config/storage';
import { cloneProviderConfig } from './providerClone';

describe('cloneProviderConfig', () => {
  test('copies provider configuration with a new id and without stale health state', () => {
    const source: IProvider = {
      id: 'prov_source',
      platform: 'openai',
      name: 'OpenRouter',
      base_url: 'https://openrouter.ai/api/v1',
      api_key: 'key-a,key-b',
      models: ['openai/gpt-4o', 'anthropic/claude-sonnet-4'],
      enabled: true,
      model_protocols: { 'anthropic/claude-sonnet-4': 'anthropic' },
      model_enabled: { 'openai/gpt-4o': false },
      model_context_limits: { 'openai/gpt-4o': 128000 },
      model_descriptions: { 'openai/gpt-4o': 'fast general model' },
      model_health: {
        'openai/gpt-4o': {
          status: 'unhealthy',
          last_check: 123,
          error: 'old error',
        },
      },
      is_full_url: false,
    };

    const clone = cloneProviderConfig(source, 'prov_copy', '副本');

    expect(clone).toMatchObject({
      ...source,
      id: 'prov_copy',
      name: 'OpenRouter 副本',
      model_health: undefined,
    });
    expect(clone.api_key).toBe(source.api_key);
    expect(clone.models).toEqual(source.models);
    expect(source.model_health).toBeDefined();
  });
});
