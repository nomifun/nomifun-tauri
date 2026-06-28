/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IMessagePlan, TMessage } from '@/common/chat/chatLib';
import { derivePinnedPlan } from './pinnedPlanModel';

type PlanEntry = IMessagePlan['content']['entries'][number];

function planMsg(entries: PlanEntry[], id = 'p1'): IMessagePlan {
  return {
    id,
    type: 'plan',
    conversation_id: 1,
    content: { session_id: 's1', entries },
  };
}

function textMsg(id = 't1'): TMessage {
  return {
    id,
    type: 'text',
    conversation_id: 1,
    content: { content: 'hi' },
  } as TMessage;
}

describe('derivePinnedPlan', () => {
  test('returns null when there is no plan message', () => {
    expect(derivePinnedPlan([textMsg('a'), textMsg('b')])).toBeNull();
  });

  test('returns null for an empty list', () => {
    expect(derivePinnedPlan([])).toBeNull();
  });

  test('returns null when the latest plan has no entries', () => {
    expect(derivePinnedPlan([planMsg([])])).toBeNull();
  });

  test('counts completed entries; in_progress and pending are not done', () => {
    const result = derivePinnedPlan([
      planMsg([
        { content: 'a', status: 'completed' },
        { content: 'b', status: 'in_progress' },
        { content: 'c', status: 'pending' },
        { content: 'd', status: 'completed' },
      ]),
    ]);
    expect(result).not.toBeNull();
    expect(result!.total).toBe(4);
    expect(result!.done).toBe(2);
    expect(result!.entries).toHaveLength(4);
  });

  test('uses the last (latest) plan when several exist', () => {
    const result = derivePinnedPlan([
      planMsg([{ content: 'old', status: 'pending' }], 'p-old'),
      textMsg('mid'),
      planMsg(
        [
          { content: 'new1', status: 'completed' },
          { content: 'new2', status: 'pending' },
        ],
        'p-new'
      ),
    ]);
    expect(result).not.toBeNull();
    expect(result!.total).toBe(2);
    expect(result!.done).toBe(1);
    expect(result!.entries[0].content).toBe('new1');
  });
});
