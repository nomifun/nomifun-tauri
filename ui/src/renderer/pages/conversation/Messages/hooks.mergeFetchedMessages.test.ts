/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { TMessage } from '@/common/chat/chatLib';
import { composeMessageForTest, mergeFetchedMessagesForConversation, mergeThinkingStreamContent } from './hooks';

const baseMessage = (overrides: Partial<TMessage>): TMessage =>
  ({
    id: 'msg',
    msg_id: 'msg',
    type: 'text',
    position: 'left',
    status: 'finish',
    hidden: false,
    conversation_id: 53,
    created_at: 1000,
    content: { content: '' },
    ...overrides,
  }) as TMessage;

describe('mergeFetchedMessagesForConversation', () => {
  test('dedupes persisted thinking against the in-flight streaming thinking with the same msg_id', () => {
    const streamingThinking = baseMessage({
      id: 'client-streaming-thinking-id',
      msg_id: 'assistant-turn-1',
      type: 'thinking',
      content: {
        content: '用户要求写一个贪吃蛇的游戏。',
        status: 'thinking',
      },
    });
    const persistedThinking = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'thinking',
      content: {
        content: '用户要求写一个贪吃蛇的游戏。',
        status: 'done',
        duration: 25408,
      },
    });

    const merged = mergeFetchedMessagesForConversation([streamingThinking], [persistedThinking], 53);

    expect(merged).toHaveLength(1);
    expect(merged[0]).toEqual(persistedThinking);
  });

  test('keeps a longer streaming thinking snapshot if the fetched row is stale', () => {
    const streamingThinking = baseMessage({
      id: 'client-streaming-thinking-id',
      msg_id: 'assistant-turn-1',
      type: 'thinking',
      content: {
        content: '用户要求写一个贪吃蛇的游戏。让我继续补充完整计划。',
        status: 'thinking',
      },
    });
    const stalePersistedThinking = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'thinking',
      content: {
        content: '用户要求写一个贪吃蛇的游戏。',
        status: 'thinking',
      },
    });

    const merged = mergeFetchedMessagesForConversation([streamingThinking], [stalePersistedThinking], 53);

    expect(merged).toHaveLength(1);
    expect(merged[0]).toEqual(streamingThinking);
  });
});

describe('composeMessageForTest', () => {
  test('keeps live agent status separate from text sharing the same turn msg_id', () => {
    const text = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: { content: 'I am already visible.' },
    });
    const status = baseMessage({
      id: 'assistant-turn-1:agent_status:model_activity',
      msg_id: 'assistant-turn-1',
      type: 'agent_status',
      position: 'left',
      content: { backend: 'nomi', status: 'preparing', agent_name: 'Nomi' },
    });

    const merged = composeMessageForTest(status, [text]);

    expect(merged).toHaveLength(2);
    expect(merged[0]).toEqual(text);
    expect(merged[1]).toEqual(status);
  });

  test('updates the same live agent status lifecycle without appending duplicates', () => {
    const status = baseMessage({
      id: 'assistant-turn-1:agent_status:model_activity',
      msg_id: 'assistant-turn-1',
      type: 'agent_status',
      position: 'left',
      content: { backend: 'nomi', status: 'preparing', agent_name: 'Nomi' },
    });
    const updated = {
      ...status,
      created_at: 2000,
      content: { backend: 'nomi', status: 'prepared', agent_name: 'Nomi' },
    } as TMessage;

    const merged = composeMessageForTest(updated, [status]);

    expect(merged).toHaveLength(1);
    expect(merged[0]).toEqual(updated);
  });
});

describe('mergeThinkingStreamContent', () => {
  test('appends normal delta chunks', () => {
    expect(mergeThinkingStreamContent('用户要求', '写一个贪吃蛇游戏')).toBe('用户要求写一个贪吃蛇游戏');
  });

  test('replaces with cumulative chunks instead of duplicating the same paragraph', () => {
    expect(mergeThinkingStreamContent('用户要求写一个贪吃蛇游戏', '用户要求写一个贪吃蛇游戏')).toBe(
      '用户要求写一个贪吃蛇游戏'
    );
    expect(mergeThinkingStreamContent('用户要求写一个贪吃蛇游戏', '用户要求写一个贪吃蛇游戏。开始创建文件')).toBe(
      '用户要求写一个贪吃蛇游戏。开始创建文件'
    );
  });

  test('treats whitespace-only formatting changes as the same cumulative snapshot', () => {
    expect(
      mergeThinkingStreamContent(
        '用户要求我写一个贪吃蛇游戏，包括：\n\n1. 游戏窗口\n2. 蛇的移动',
        '用户要求我写一个贪吃蛇游戏，包括： 1. 游戏窗口 2. 蛇的移动'
      )
    ).toBe('用户要求我写一个贪吃蛇游戏，包括：\n\n1. 游戏窗口\n2. 蛇的移动');
  });

  test('ignores shorter replayed thinking snapshots after whitespace normalization', () => {
    expect(
      mergeThinkingStreamContent(
        '用户要求我写一个贪吃蛇游戏，包括：\n\n1. 游戏窗口\n2. 蛇的移动\n3. 食物生成',
        '用户要求我写一个贪吃蛇游戏，包括： 1. 游戏窗口'
      )
    ).toBe('用户要求我写一个贪吃蛇游戏，包括：\n\n1. 游戏窗口\n2. 蛇的移动\n3. 食物生成');
  });
});
