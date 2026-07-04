/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { providerErrorI18nKey } from './conversationCreateError';

describe('providerErrorI18nKey', () => {
  test('maps PROVIDER_UNAVAILABLE', () => {
    expect(providerErrorI18nKey('PROVIDER_UNAVAILABLE')).toBe('conversation.agentError.codes.PROVIDER_UNAVAILABLE.body');
  });
  test('returns undefined for unrelated codes', () => {
    expect(providerErrorI18nKey('BAD_REQUEST')).toBeUndefined();
  });
});
