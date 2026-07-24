/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseProviderId } from '@/common/types/ids';
import {
  fromProviderResponse,
  toCreateProviderRequest,
  type ProviderResponse,
} from './providerApi';

const PROVIDER_ID = '0190f5fe-7c00-7a00-8000-000000000002';

const expectThrow = (action: () => unknown) => {
  try {
    action();
  } catch {
    return;
  }
  throw new Error('Expected action to throw');
};

const response = (provider_id: string): ProviderResponse => ({
  provider_id,
  platform: 'openai',
  name: 'OpenAI',
  base_url: 'https://api.openai.com',
  api_key: 'sk-test',
  models: ['gpt-4o'],
  enabled: true,
  capabilities: [],
  model_context_limits: { 'gpt-4o': 128_000 },
  is_full_url: false,
  sort_order: 0,
  created_at: 1,
  updated_at: 1,
});

describe('provider wire contract', () => {
  test('maps provider_id responses to the internal id field', () => {
    const provider = fromProviderResponse(response(PROVIDER_ID));

    expect(provider.id).toBe(parseProviderId(PROVIDER_ID));
    expect(provider.model_context_limits).toEqual({ 'gpt-4o': 128_000 });
    expect(Object.prototype.hasOwnProperty.call(provider, 'provider_id')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(provider, 'context_limit')).toBe(false);
  });

  test('rejects non-canonical provider ids at the wire boundary', () => {
    expectThrow(() => fromProviderResponse(response(`prov_${PROVIDER_ID}`)));
    expectThrow(() => fromProviderResponse(response(PROVIDER_ID.toUpperCase())));
  });

  test('renames the internal create id to provider_id without sending id', () => {
    const request = toCreateProviderRequest({
      ...fromProviderResponse(response(PROVIDER_ID)),
      id: parseProviderId(PROVIDER_ID),
    });

    expect(request.provider_id).toBe(parseProviderId(PROVIDER_ID));
    expect(request.model_context_limits).toEqual({ 'gpt-4o': 128_000 });
    expect(Object.prototype.hasOwnProperty.call(request, 'id')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(request, 'context_limit')).toBe(false);
  });
});
