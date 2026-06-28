/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMessagePlan, TMessage } from '@/common/chat/chatLib';

export interface PinnedPlanData {
  entries: IMessagePlan['content']['entries'];
  /** Number of entries with status === 'completed'. */
  done: number;
  /** Total number of entries. */
  total: number;
}

/**
 * Derive the data for the pinned plan bar from the conversation message list.
 *
 * The current plan is the last `plan` message in the list (plan updates reuse
 * the same `msg_id` and are moved to the tail by the message-compose logic, so
 * the tail-most plan is always the freshest). Returns `null` when there is no
 * plan or the latest plan carries no entries — in both cases the bar hides.
 */
export function derivePinnedPlan(list: TMessage[]): PinnedPlanData | null {
  for (let i = list.length - 1; i >= 0; i--) {
    const message = list[i];
    if (message.type !== 'plan') continue;
    const entries = message.content.entries ?? [];
    if (entries.length === 0) return null;
    const done = entries.filter((entry) => entry.status === 'completed').length;
    return { entries, done, total: entries.length };
  }
  return null;
}
