/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { normalizeApiKeyList, parseApiKeyList, validateApiKeysForSave } from './apiKeys';

describe('API key list helpers', () => {
  test('parses comma separated API keys', () => {
    expect(parseApiKeyList('key-a, key-b,,key-c')).toEqual(['key-a', 'key-b', 'key-c']);
  });

  test('normalizes legacy newline separated API keys to comma separated storage', () => {
    expect(normalizeApiKeyList('key-a\nkey-b\r\n key-c ')).toBe('key-a,key-b,key-c');
  });

  test('validates every API key and reports invalid indexes before save', async () => {
    const checked: string[] = [];
    const result = await validateApiKeysForSave('key-a,key-b,key-c', async (key) => {
      checked.push(key);
      return key !== 'key-b';
    });

    expect(checked).toEqual(['key-a', 'key-b', 'key-c']);
    expect(result.valid).toBe(false);
    expect(result.invalidIndexes).toEqual([1]);
    expect(result.normalized).toBe('key-a,key-b,key-c');
  });

  test('passes save validation only when all API keys are usable', async () => {
    const result = await validateApiKeysForSave('key-a, key-b', async () => true);

    expect(result.valid).toBe(true);
    expect(result.invalidIndexes).toEqual([]);
    expect(result.normalized).toBe('key-a,key-b');
  });
});
