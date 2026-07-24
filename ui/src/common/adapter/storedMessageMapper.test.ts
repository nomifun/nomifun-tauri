/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  createStoredMessageMapper,
  type StoredMessageResponse,
} from './storedMessageMapper';

const MESSAGE_ID = '019b0000-0000-7000-8000-000000000001';
const CONVERSATION_ID = '019b0000-0000-7000-8000-000000000002';
const STREAM_ID = '019b0000-0000-7000-8000-000000000003';
const TURN_ID = '019b0000-0000-7000-8000-000000000004';

const rawMessage = (
  overrides: Partial<StoredMessageResponse> = {}
): StoredMessageResponse => ({
  message_id: MESSAGE_ID,
  conversation_id: CONVERSATION_ID,
  msg_id: STREAM_ID,
  type: 'text',
  content: { content: 'hello', turn_id: TURN_ID },
  position: 'left',
  status: 'finish',
  hidden: false,
  created_at: 1,
  ...overrides,
});

const expectRejected = (callback: () => unknown) => {
  let error: unknown;
  try {
    callback();
  } catch (caught) {
    error = caught;
  }
  expect(error instanceof Error).toBe(true);
};

describe('stored message v3 mapper', () => {
  test('separates durable message identity from a stable renderer-local key', () => {
    let sequence = 0;
    const map = createStoredMessageMapper(() => `render-${++sequence}`);

    const first = map(rawMessage());
    const repeated = map(rawMessage({ content: { content: 'updated' } }));

    expect(first.id).toBe('render-1');
    expect(repeated.id).toBe(first.id);
    expect(first.message_id).toBe(MESSAGE_ID);
    expect(first.msg_id).toBe(STREAM_ID);
    expect(first.conversation_id).toBe(CONVERSATION_ID);
  });

  test('projects content.turn_id exactly once and removes it from renderer content', () => {
    const map = createStoredMessageMapper(() => 'render-1');
    const mapped = map(rawMessage());

    expect(mapped.turn_id).toBe(TURN_ID);
    expect(mapped.content).toEqual({ content: 'hello' });
    expect('turn_id' in mapped.content).toBe(false);
  });

  test('ignores an unsupported top-level turn_id instead of reading two authorities', () => {
    const map = createStoredMessageMapper(() => 'render-1');
    const mapped = map({
      ...rawMessage({ content: { content: 'hello' } }),
      turn_id: TURN_ID,
    } as StoredMessageResponse & { turn_id: string });

    expect(mapped.turn_id).toBeUndefined();
  });

  test('rejects prefixed, uppercase, non-v7, and malformed nested business ids', () => {
    const invalidIds = [
      `msg_${MESSAGE_ID}`,
      MESSAGE_ID.toUpperCase(),
      '019b0000-0000-4000-8000-000000000001',
    ];

    for (const id of invalidIds) {
      expectRejected(() =>
        createStoredMessageMapper(() => 'render-1')(rawMessage({ message_id: id }))
      );
    }
    expectRejected(() =>
      createStoredMessageMapper(() => 'render-1')(
        rawMessage({ content: { content: 'hello', turn_id: 'turn-external' } })
      )
    );
  });

  test('rejects the removed generic wire id', () => {
    const { message_id: _messageId, ...legacy } = rawMessage();
    expectRejected(() =>
      createStoredMessageMapper(() => 'render-1')({
        ...legacy,
        id: MESSAGE_ID,
      } as unknown as StoredMessageResponse)
    );
  });

  test('rejects guessed metadata enums, non-finite timestamps, and string JSON content', () => {
    const map = createStoredMessageMapper(() => 'render-1');

    expectRejected(() => map(rawMessage({ position: 'assistant' })));
    expectRejected(() => map(rawMessage({ status: 'complete' })));
    expectRejected(() => map(rawMessage({ created_at: Number.NaN })));
    expectRejected(() => map(rawMessage({ content: JSON.stringify({ content: 'old' }) })));
    expectRejected(() => map(rawMessage({ type: 'tool_group', content: { call_id: 'call-1' } })));
  });
});
