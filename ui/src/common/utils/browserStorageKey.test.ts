/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { conversationTarget, parseConversationId, terminalTarget } from '@/common/types/ids';
import {
  browserStorageGenerationKey,
  browserStorageKey,
  sessionStorageKey,
  setBrowserStorageGeneration,
} from './browserStorageKey';

describe('browser storage keys', () => {
  test('includes schema version and entity namespace', () => {
    setBrowserStorageGeneration('01900000-0000-7000-8000-000000000000');
    const conversationKey = sessionStorageKey(
      'workspace-panel-tab',
      conversationTarget('0190f5fe-7c00-7a00-8000-000000000001'),
    );
    const terminalKey = sessionStorageKey(
      'workspace-panel-tab',
      terminalTarget('0190f5fe-7c00-7a00-8000-000000000001'),
    );

    expect(conversationKey.includes('|v1|')).toBe(true);
    expect(conversationKey).not.toBe(terminalKey);
  });

  test('length-prefixes segments so concatenation boundaries cannot collide', () => {
    const left = browserStorageKey(
      'ab',
      'conversation',
      parseConversationId('0190f5fe-7c00-7a00-8000-000000000001'),
    );
    const right = browserStorageKey(
      'a',
      'conversation',
      parseConversationId('0190f5fe-7c00-7a00-8000-000000000002'),
    );

    expect(left).not.toBe(right);
  });

  test('rotates when the backend dataset generation changes', () => {
    setBrowserStorageGeneration('01900000-0000-7000-8000-000000000001');
    const before = sessionStorageKey(
      'draft',
      conversationTarget('0190f5fe-7c00-7a00-8000-000000000001'),
    );
    setBrowserStorageGeneration('01900000-0000-7000-8000-000000000002');
    const after = sessionStorageKey(
      'draft',
      conversationTarget('0190f5fe-7c00-7a00-8000-000000000002'),
    );

    expect(before).not.toBe(after);
  });

  test('provides generation-scoped global feature keys without reading old keys', () => {
    setBrowserStorageGeneration('01900000-0000-7000-8000-000000000003');
    const key = browserStorageGenerationKey('cron-unread');

    expect(key.includes('|v1|')).toBe(true);
    expect(key.includes('cron-unread')).toBe(true);
    expect(key).not.toBe('nomifun_cron_unread');
  });

  test('rejects malformed or non-v7 storage generations', () => {
    for (const value of [
      '',
      '01900000-0000-4000-8000-000000000001',
      '01900000-0000-7000-8000-000000000001 ',
      '01900000-0000-7000-C000-000000000001',
    ]) {
      let error: unknown;
      try {
        setBrowserStorageGeneration(value);
      } catch (caught) {
        error = caught;
      }
      expect(error instanceof TypeError).toBe(true);
    }
  });
});
