/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { isNewApiPlatform, platformHasNoModelsEndpoint } from './platformConstants';

describe('platformHasNoModelsEndpoint', () => {
  test('true for subscription plan gateways without a /models catalog', () => {
    // These endpoints only route /chat/completions; probing /models 404s and
    // would wrongly reject a valid subscription key at save time.
    expect(platformHasNoModelsEndpoint('ark-agent-plan')).toBe(true);
    expect(platformHasNoModelsEndpoint('ark-coding-plan')).toBe(true);
    expect(platformHasNoModelsEndpoint('glm-coding-plan')).toBe(true);
    expect(platformHasNoModelsEndpoint('qianfan-coding-plan')).toBe(true);
    expect(platformHasNoModelsEndpoint('stepfun-plan')).toBe(true);
    expect(platformHasNoModelsEndpoint('dashscope-coding')).toBe(true);
    expect(platformHasNoModelsEndpoint('minimax-coding-plan')).toBe(true);
  });

  test('false for ordinary OpenAI-compatible platforms that expose /models', () => {
    expect(platformHasNoModelsEndpoint('custom')).toBe(false);
    expect(platformHasNoModelsEndpoint('openai')).toBe(false);
    expect(platformHasNoModelsEndpoint('ark')).toBe(false);
    expect(platformHasNoModelsEndpoint('deepseek')).toBe(false);
    expect(platformHasNoModelsEndpoint('new-api')).toBe(false);
  });

  test('false for empty / nullish input', () => {
    expect(platformHasNoModelsEndpoint('')).toBe(false);
    expect(platformHasNoModelsEndpoint(undefined)).toBe(false);
    expect(platformHasNoModelsEndpoint(null)).toBe(false);
  });
});

describe('isNewApiPlatform', () => {
  test('identifies the new-api gateway', () => {
    expect(isNewApiPlatform('new-api')).toBe(true);
    expect(isNewApiPlatform('custom')).toBe(false);
  });
});
