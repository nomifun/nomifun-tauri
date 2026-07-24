/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseConversationId } from '@/common/types/ids';
import {
  buildCronConversationRequestFields,
  resolveCronConversationTarget,
} from './cronConversationTarget';

describe('resolveCronConversationTarget', () => {
  const conversationId = parseConversationId('019b0000-0000-7000-8000-000000000042');

  test.each(['new_conversation', 'existing'] as const)(
    '%s starts unbound and ignores stale picker state',
    (executionMode) => {
      const target = resolveCronConversationTarget(executionMode, conversationId);
      expect(target).toEqual({
        kind: 'unbound',
        executionMode,
      });
      expect(buildCronConversationRequestFields(target!)).toEqual({ execution_mode: executionMode });
      expect('conversation_id' in buildCronConversationRequestFields(target!)).toBe(false);
    },
  );

  test('specified mode requires and preserves a canonical conversation ID', () => {
    expect(resolveCronConversationTarget('specified')).toBeNull();
    const target = resolveCronConversationTarget('specified', conversationId);
    expect(target).toEqual({
      kind: 'specified',
      executionMode: 'existing',
      conversationId,
    });
    expect(buildCronConversationRequestFields(target!)).toEqual({
      execution_mode: 'existing',
      conversation_id: conversationId,
    });
  });
});
