/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseScheduledConversationId } from './scheduledConversationId';

describe('parseScheduledConversationId', () => {
  const conversationId = '0190f5fe-7c00-7a00-8000-000000000042';

  test('parses a locked conversation id from route search params', () => {
    const parsed = parseScheduledConversationId(
      new URLSearchParams(`create=conversation&conversation_id=${conversationId}`),
    );

    expect(parsed).toBe(conversationId);
  });

  test('ignores invalid or incomplete conversation ids', () => {
    expect(parseScheduledConversationId(new URLSearchParams('create=conversation'))).toBeNull();
    expect(
      parseScheduledConversationId(
        new URLSearchParams(`create=conversation&conversationId=${conversationId}`),
      ),
    ).toBeNull();
    expect(
      parseScheduledConversationId(new URLSearchParams('create=conversation&conversation_id=abc')),
    ).toBeNull();
    expect(
      parseScheduledConversationId(
        new URLSearchParams(`create=unknown&conversation_id=${conversationId}`),
      ),
    ).toBeNull();
  });
});
