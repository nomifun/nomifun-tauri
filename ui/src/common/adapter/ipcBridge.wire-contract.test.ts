/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { fromApiTurnCompletedEvent } from './ipcBridge';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');
const CONVERSATION_ID = '0190f5fe-7c00-7a00-8000-000000000001';
const MESSAGE_ID = '0190f5fe-7c00-7a00-8000-000000000002';

describe('ipc bridge wire ID contracts', () => {
  test('revoke user uses channel_user_id and not user_id', () => {
    expect(source.includes('revokeUser: httpPost<void, { channel_user_id:')).toBe(true);
    expect(source.includes("'/api/channel/users/revoke'")).toBe(true);
    expect(source.includes('revokeUser: httpPost<void, { user_id:')).toBe(false);
  });

  test('turn.completed last_message uses message_id and rejects generic id', () => {
    expect(source.includes('message_id?: MessageId;')).toBe(true);
    expect(source.includes('last_message legacy field "id" is not accepted')).toBe(true);
    expect(source.includes('          id: rawLast.id')).toBe(false);
  });

  test('maps message_id and rejects last_message.id at runtime', () => {
    const mapped = fromApiTurnCompletedEvent({
      conversation_id: CONVERSATION_ID,
      turn_id: MESSAGE_ID,
      last_message: {
        message_id: MESSAGE_ID,
        content: 'done',
        created_at: 1,
      },
    });
    expect(mapped.last_message.message_id).toBe(MESSAGE_ID);

    let rejected = false;
    try {
      fromApiTurnCompletedEvent({
        conversation_id: CONVERSATION_ID,
        turn_id: MESSAGE_ID,
        last_message: {
          id: MESSAGE_ID,
          content: 'legacy',
          created_at: 1,
        },
      });
    } catch {
      rejected = true;
    }
    expect(rejected).toBe(true);
  });

  test('manual knowledge writeback retry uses the owning conversation and message IDs', () => {
    expect(
      /retryKnowledgeWriteback:\s*httpPost<\s*void,\s*\{\s*conversation_id:\s*ConversationId;\s*message_id:\s*MessageId;\s*attempt_id:\s*string;?\s*\}\s*>/.test(
        source
      )
    ).toBe(true);
    expect(
      source.includes(
        '`/api/conversations/${p.conversation_id}/messages/${p.message_id}/knowledge-writeback/retry`'
      )
    ).toBe(true);
  });
});
