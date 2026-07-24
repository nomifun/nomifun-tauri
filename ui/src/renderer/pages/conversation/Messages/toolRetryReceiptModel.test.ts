/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { ExplicitToolRetryReceiptIndex } from './toolRetryReceiptModel';

const message = (
  callId: string,
  attemptNo: number,
  options: { turnId?: string; name?: string; retryOf?: string } = {}
) =>
  ({
    type: 'tool_call',
    id: callId,
    turn_id: options.turnId ?? 'turn-1',
    content: {
      call_id: callId,
      name: options.name ?? 'nomi_delegate',
      retry: {
        retry_group_id: 'call-1',
        attempt_no: attemptNo,
        ...(options.retryOf ? { retry_of_call_id: options.retryOf } : {}),
      },
    },
  }) as any;

describe('ExplicitToolRetryReceiptIndex', () => {
  test('keeps an explicit chain across non-tool messages', () => {
    const index = new ExplicitToolRetryReceiptIndex<object>();
    const receipt = {};
    index.rememberFirst(message('call-1', 1), receipt);

    // A thinking/text item is intentionally not passed to the tool index.
    expect(index.takeContinuation(message('call-2', 2, { retryOf: 'call-1' }))).toBe(receipt);
  });

  test('fails closed across turns, names, and broken links', () => {
    const index = new ExplicitToolRetryReceiptIndex<object>();
    index.rememberFirst(message('call-1', 1), {});

    expect(index.takeContinuation(message('call-2', 2, { turnId: 'turn-2', retryOf: 'call-1' }))).toBeUndefined();
    expect(index.takeContinuation(message('call-2', 2, { name: 'other', retryOf: 'call-1' }))).toBeUndefined();
    expect(index.takeContinuation(message('call-2', 2, { retryOf: 'call-1' }))).toBeUndefined();
    expect(index.takeContinuation(message('call-2', 3, { retryOf: 'call-1' }))).toBeUndefined();
    expect(index.takeContinuation(message('call-2', 2, { retryOf: 'wrong' }))).toBeUndefined();
  });

  test('never reopens a retry group after a duplicate root or malformed link', () => {
    const duplicateRoot = new ExplicitToolRetryReceiptIndex<object>();
    const firstReceipt = {};
    const duplicateReceipt = {};
    duplicateRoot.rememberFirst(message('call-1', 1), firstReceipt);
    expect(duplicateRoot.takeContinuation(message('call-1', 1))).toBeUndefined();
    duplicateRoot.rememberFirst(message('call-1', 1), duplicateReceipt);
    expect(
      duplicateRoot.takeContinuation(message('call-2', 2, { retryOf: 'call-1' }))
    ).toBeUndefined();

    const broken = new ExplicitToolRetryReceiptIndex<object>();
    broken.rememberFirst(message('call-1', 1), firstReceipt);
    expect(broken.takeContinuation(message('call-2', 3, { retryOf: 'call-1' }))).toBeUndefined();
    expect(
      broken.takeContinuation(message('call-2', 2, { retryOf: 'call-1' }))
    ).toBeUndefined();
  });
});
