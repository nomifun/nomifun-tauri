/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseEntityId, tryParseEntityId } from '@/common/types/ids';
import { uuidv7 } from './uuidv7';

const CANONICAL_UUID_V7 =
  /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

describe('uuidv7', () => {
  test('mints canonical lowercase UUIDv7 values with RFC version and variant bits', () => {
    for (let index = 0; index < 32; index += 1) {
      const id = uuidv7();
      expect(parseEntityId('provider', id)).toBe(id);
      expect(id.length).toBe(36);
      expect(CANONICAL_UUID_V7.test(id)).toBe(true);
      expect(id[14]).toBe('7');
      expect('89ab'.includes(id[19] ?? '')).toBe(true);
      expect(id).toBe(id.toLowerCase());
    }
  });

  test('mints unique values across a batch', () => {
    const ids = Array.from({ length: 512 }, () => uuidv7());
    expect(new Set(ids).size).toBe(ids.length);
  });

  test('legacy prefixed IDs are rejected at the entity boundary', () => {
    expect(
      tryParseEntityId('provider', 'prov_0190f5fe-7c00-7a00-8000-000000000001'),
    ).toBeNull();
  });
});
