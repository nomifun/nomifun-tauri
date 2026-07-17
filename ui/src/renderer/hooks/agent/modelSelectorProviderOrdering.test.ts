import { describe, expect, test } from 'bun:test';

import { NOMIFUN_FREE_MODEL_PLATFORM } from '@/common/types/provider/managedModelService';
import { orderModelSelectorProviders } from './modelSelectorProviderOrdering';

type ProviderStub = {
  name: string;
  platform: string;
};

const provider = (name: string, platform = 'openai'): ProviderStub => ({ name, platform });

describe('orderModelSelectorProviders', () => {
  test('places configured providers before free managed models', () => {
    const result = orderModelSelectorProviders([
      provider('Free', NOMIFUN_FREE_MODEL_PLATFORM),
      provider('Provider A'),
      provider('Provider B', 'anthropic'),
    ]);

    expect(result.map(({ name }) => name)).toEqual(['Provider A', 'Provider B', 'Free']);
  });

  test('preserves provider priority inside each model category', () => {
    const input = [
      provider('Provider B', 'anthropic'),
      provider('Free', NOMIFUN_FREE_MODEL_PLATFORM),
      provider('Provider A'),
    ];

    const result = orderModelSelectorProviders(input);

    expect(result.map(({ name }) => name)).toEqual(['Provider B', 'Provider A', 'Free']);
    expect(input.map(({ name }) => name)).toEqual(['Provider B', 'Free', 'Provider A']);
  });
});
