/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMessageToolCall } from '@/common/chat/chatLib';

interface RetryReceiptEntry<T> {
  receipt: T;
  name: string;
  lastCallId: string;
  lastAttemptNo: number;
}

const retryKey = (message: IMessageToolCall): string | undefined => {
  const retry = message.content.retry;
  if (
    !message.turn_id ||
    !retry ||
    typeof retry.retry_group_id !== 'string' ||
    retry.retry_group_id.length === 0 ||
    !Number.isInteger(retry.attempt_no) ||
    retry.attempt_no < 1
  ) {
    return undefined;
  }
  return `${message.turn_id}:${retry.retry_group_id}`;
};

/**
 * Indexes explicit retry chains without depending on message adjacency. Text
 * or thinking between attempts does not mutate the index, while the turn id,
 * exact tool name, attempt number and previous call id must all match.
 */
export class ExplicitToolRetryReceiptIndex<T> {
  private readonly entries = new Map<string, RetryReceiptEntry<T>>();
  private readonly closedKeys = new Set<string>();

  takeContinuation(message: IMessageToolCall): T | undefined {
    const key = retryKey(message);
    const retry = message.content.retry;
    if (!key || !retry || this.closedKeys.has(key)) return undefined;

    const existing = key ? this.entries.get(key) : undefined;
    const isRoot =
      retry.attempt_no === 1 &&
      retry.retry_group_id === message.content.call_id &&
      retry.retry_of_call_id === undefined;
    if (isRoot) {
      if (existing) {
        // A second root claiming the same identity makes every later link
        // ambiguous. Keep both receipts separate and never reopen this key.
        this.entries.delete(key);
        this.closedKeys.add(key);
      }
      return undefined;
    }

    if (
      !existing ||
      existing.name !== message.content.name ||
      retry.attempt_no !== existing.lastAttemptNo + 1 ||
      retry.retry_of_call_id !== existing.lastCallId
    ) {
      this.entries.delete(key);
      this.closedKeys.add(key);
      return undefined;
    }

    existing.lastCallId = message.content.call_id;
    existing.lastAttemptNo = retry.attempt_no;
    return existing.receipt;
  }

  rememberFirst(message: IMessageToolCall, receipt: T): void {
    const key = retryKey(message);
    const retry = message.content.retry;
    if (
      !key ||
      this.closedKeys.has(key) ||
      !retry ||
      retry.attempt_no !== 1 ||
      retry.retry_group_id !== message.content.call_id ||
      retry.retry_of_call_id !== undefined
    ) {
      return;
    }
    if (this.entries.has(key)) {
      // Two roots claiming the same group make the chain ambiguous.
      this.entries.delete(key);
      this.closedKeys.add(key);
      return;
    }
    this.entries.set(key, {
      receipt,
      name: message.content.name,
      lastCallId: message.content.call_id,
      lastAttemptNo: 1,
    });
  }
}
