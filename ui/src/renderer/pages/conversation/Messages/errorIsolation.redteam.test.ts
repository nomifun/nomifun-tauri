/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { TMessage } from '@/common/chat/chatLib';
import { parseConversationId, parseMessageId, type MessageId } from '@/common/types/ids';
import {
  composeMessageForTest,
  mergeFetchedMessagesForConversation,
} from './hooks';

const CONVERSATION_ID = parseConversationId(
  '019b0000-0000-7000-8000-000000000001'
);

const messageId = (sequence: number): MessageId =>
  parseMessageId(
    `019b0000-0000-7000-8000-${sequence.toString(16).padStart(12, '0')}`
  );

const durableMessageId = (label: string): MessageId => {
  let hash = 0xcbf29ce484222325n;
  for (const char of label) {
    hash ^= BigInt(char.codePointAt(0) ?? 0);
    hash = BigInt.asUintN(48, hash * 0x100000001b3n);
  }
  return parseMessageId(`019b0000-0000-7001-8000-${hash.toString(16).padStart(12, '0')}`);
};

const fetchedMessage = <T extends TMessage>(message: T): T =>
  ({
    ...message,
    message_id: message.message_id ?? durableMessageId(String(message.id)),
  }) as T;

const textMessage = (
  id: string,
  msgId: MessageId,
  createdAt: number,
  content: string
): TMessage =>
  ({
    id,
    msg_id: msgId,
    conversation_id: CONVERSATION_ID,
    type: 'text',
    position: 'left',
    status: 'finish',
    hidden: false,
    created_at: createdAt,
    content: { content },
  }) as TMessage;

const errorMessage = (
  id: string,
  turnId: MessageId,
  createdAt: number,
  content: string
): TMessage =>
  ({
    id,
    msg_id: turnId,
    conversation_id: CONVERSATION_ID,
    type: 'tips',
    position: 'center',
    status: 'error',
    hidden: false,
    created_at: createdAt,
    content: {
      content,
      type: 'error',
      error: {
        message: content,
        code: 'USER_LLM_PROVIDER_RATE_LIMITED',
        ownership: 'user_llm_provider',
      },
    },
  }) as TMessage;

describe('conversation error isolation red-team contracts', () => {
  test('a terminal newest-window refresh keeps persisted older pages in chronological position', () => {
    const olderA = textMessage('persisted-older-a', messageId(1), 100, 'older a');
    const olderB = textMessage('persisted-older-b', messageId(2), 200, 'older b');
    const staleNewest = textMessage('persisted-newest', messageId(3), 300, 'old snapshot');
    const refreshedNewest = textMessage(
      'persisted-newest',
      messageId(3),
      300,
      'authoritative snapshot'
    );

    const merged = mergeFetchedMessagesForConversation(
      [olderA, olderB, staleNewest],
      [fetchedMessage(refreshedNewest)],
      CONVERSATION_ID
    );

    expect(merged.map((message) => message.id)).toEqual([
      olderA.id,
      olderB.id,
      refreshedNewest.id,
    ]);
    const newest = merged[2];
    expect(newest.type).toBe('text');
    if (newest.type !== 'text') {
      throw new Error('expected the refreshed assistant text row');
    }
    expect(newest.content.content).toBe('authoritative snapshot');
  });

  test('one turn has exactly one error after its live frame is persisted', () => {
    const turnId = messageId(10);
    const live = errorMessage('client-live-error', turnId, 500, 'rate limited');
    const persisted = fetchedMessage(errorMessage('persisted-error-row', turnId, 500, 'rate limited'));

    const merged = mergeFetchedMessagesForConversation(
      [live],
      [persisted],
      CONVERSATION_ID
    );

    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe(persisted.id);
    expect(merged[0].type).toBe('tips');
  });

  test('text and a terminal error sharing a turn id remain distinct renderer rows', () => {
    const turnId = messageId(20);
    const answer = textMessage('assistant-answer', turnId, 700, 'completed answer');
    const terminalError = errorMessage('terminal-error', turnId, 701, 'late error');

    const merged = composeMessageForTest(terminalError, [answer]);

    expect(merged).toHaveLength(2);
    expect(merged.map((message) => message.type)).toEqual(['text', 'tips']);
    expect(merged[0]).toEqual(answer);
    expect(merged[1]).toEqual(terminalError);
  });

  test('an unmatched live error stays visible at its original position instead of poisoning the tail', () => {
    const oldError = errorMessage('old-live-error', messageId(30), 900, 'old failure');
    const persistedAnswer = textMessage(
      'persisted-current-answer',
      messageId(31),
      1_000,
      'current answer'
    );
    const nextTurnActivity = textMessage(
      'next-turn-live',
      messageId(32),
      1_101,
      'new turn is already streaming'
    );

    const merged = mergeFetchedMessagesForConversation(
      [oldError, persistedAnswer, nextTurnActivity],
      [fetchedMessage(persistedAnswer)],
      CONVERSATION_ID
    );

    expect(merged.map((message) => message.id)).toEqual([
      oldError.id,
      persistedAnswer.id,
      nextTurnActivity.id,
    ]);
  });
});
