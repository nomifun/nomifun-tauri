/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseAuthUser } from './AuthContext';

const USER_ID = '0190f5fe-7c00-7a00-8000-000000000001';

describe('auth user wire contract', () => {
  test('maps user_id to the UI internal id', () => {
    expect(parseAuthUser({ user_id: USER_ID, username: 'admin' })).toEqual({
      id: USER_ID,
      username: 'admin',
    });
  });

  test('rejects the legacy generic id field', () => {
    expect(parseAuthUser({ id: USER_ID, username: 'admin' })).toBe(null);
  });

  test('rejects a payload containing both user_id and generic id', () => {
    expect(parseAuthUser({ user_id: USER_ID, id: USER_ID, username: 'admin' })).toBe(null);
  });
});
