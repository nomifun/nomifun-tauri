import { describe, expect, test } from 'bun:test';

import { NOMIFUN_FREE_MODEL_PLATFORM } from '@/common/types/provider/managedModelService';
import { formatModelSelectorProviderLabel } from './useModelSelectorProviderLabel';

const labels = {
  free: '免费模型',
};

describe('formatModelSelectorProviderLabel', () => {
  test('localizes the managed free provider platform', () => {
    expect(formatModelSelectorProviderLabel({ platform: NOMIFUN_FREE_MODEL_PLATFORM }, labels)).toBe('免费模型');
  });

  test('preserves ordinary provider names and falls back to the platform', () => {
    expect(formatModelSelectorProviderLabel({ name: 'OpenAI', platform: 'openai' }, labels)).toBe('OpenAI');
    expect(formatModelSelectorProviderLabel({ platform: 'anthropic' }, labels)).toBe('anthropic');
  });
});
