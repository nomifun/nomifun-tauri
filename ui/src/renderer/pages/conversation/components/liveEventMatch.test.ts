/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseConversationId, parseTerminalId } from '@/common/types/ids';
import { isLiveEventForTarget } from './liveEventMatch';

describe('isLiveEventForTarget', () => {
  test('matches the same canonical target', () => {
    const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000002');
    expect(
      isLiveEventForTarget(
        'conversation',
        conversationId,
        'conversation',
        conversationId,
      ),
    ).toBe(true);
  });

  test('does not match another entity or kind', () => {
    const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000002');
    const otherConversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000003');
    const terminalId = parseTerminalId('0190f5fe-7c00-7a00-8000-000000000002');
    expect(
      isLiveEventForTarget(
        'conversation',
        otherConversationId,
        'conversation',
        conversationId,
      ),
    ).toBe(false);
    expect(isLiveEventForTarget('terminal', terminalId, 'conversation', conversationId)).toBe(false);
  });
});
