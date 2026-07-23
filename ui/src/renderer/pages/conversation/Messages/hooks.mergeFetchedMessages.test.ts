/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseConversationId, parseCronJobId, parseMessageId, type MessageId } from '@/common/types/ids';
import type { TMessage } from '@/common/chat/chatLib';
import {
  composeMessageForTest,
  mergeFetchedMessagesForConversation,
  mergeThinkingStreamContent,
  normalizeDbMessage,
} from './hooks';
import { assignTurnIdsFromUserRequests, buildTurnDisclosureItems } from './turnDisclosureModel';

const messageId = (label: string): MessageId => {
  const suffix = Array.from(label)
    .map((char) => char.charCodeAt(0).toString(16).padStart(2, '0'))
    .join('')
    .slice(0, 12)
    .padEnd(12, '0');
  return parseMessageId(`019b0000-0000-7000-8000-${suffix}`);
};

type MessageOverrides = Omit<Partial<TMessage>, 'msg_id'> & { msg_id?: string | MessageId };

const baseMessage = (overrides: MessageOverrides): TMessage =>
  ({
    id: 'msg',
    msg_id: messageId('default'),
    type: 'text',
    position: 'left',
    status: 'finish',
    hidden: false,
    conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000004'),
    created_at: 1000,
    content: { content: '' },
    ...overrides,
    ...(overrides.msg_id == null ? {} : { msg_id: messageId(overrides.msg_id) }),
  }) as TMessage;

describe('mergeFetchedMessagesForConversation', () => {
  test('keeps a late ACP image completion before the final answer and produces one disclosure', () => {
    const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000004');
    const userId = messageId('image-user');
    const rootTurnId = messageId('image-root');
    const finalId = messageId('image-final');
    const user = normalizeDbMessage(baseMessage({
      id: userId,
      msg_id: 'image-user',
      position: 'right',
      created_at: 1000,
      content: { content: 'Generate an office image' },
    }));
    const intro = normalizeDbMessage(baseMessage({
      id: rootTurnId,
      msg_id: 'image-root',
      turn_id: rootTurnId,
      created_at: 1500,
      content: { content: 'I will generate it.' },
    }));
    const persistedTool = normalizeDbMessage(baseMessage({
      id: 'persisted-image-tool',
      msg_id: 'image-root',
      turn_id: rootTurnId,
      type: 'acp_tool_call',
      created_at: 2900,
      content: {
        session_id: 'session-image',
        artifact_delivery_committed: true,
        update: {
          session_update: 'tool_call_update',
          tool_call_id: 'image-call',
          status: 'completed',
          content: [],
        },
      } as any,
    }));
    const final = normalizeDbMessage(baseMessage({
      id: finalId,
      msg_id: 'image-final',
      turn_id: rootTurnId,
      created_at: 3000,
      content: { content: 'Image generated.' },
    }));
    const lateLiveTool = baseMessage({
      id: 'late-live-image-tool',
      msg_id: 'image-root',
      turn_id: rootTurnId,
      type: 'acp_tool_call',
      created_at: 5000,
      content: {
        session_id: 'session-image',
        update: {
          session_update: 'tool_call_update',
          tool_call_id: 'image-call',
          status: 'completed',
          content: [],
        },
      },
    } as any);
    const liveIntro = { ...intro, id: 'live-intro', turn_id: rootTurnId } as TMessage;
    const liveFinal = { ...final, id: 'live-final', turn_id: rootTurnId } as TMessage;

    const merged = mergeFetchedMessagesForConversation(
      [user, liveIntro, liveFinal, lateLiveTool],
      [user, intro, persistedTool, final],
      conversationId
    );

    expect(merged.map((message) => message.id)).toEqual([
      userId,
      rootTurnId,
      'persisted-image-tool',
      finalId,
    ]);
    expect(merged.map((message) => message.created_at)).toEqual([1000, 1500, 2900, 3000]);
    expect(merged.slice(1).map((message) => message.turn_id)).toEqual([
      rootTurnId,
      rootTurnId,
      rootTurnId,
    ]);

    const grouped = assignTurnIdsFromUserRequests(
      merged.map((message) => ({
        id: message.id,
        role:
          message.type === 'text'
            ? message.position === 'right'
              ? 'user' as const
              : 'assistant' as const
            : 'process' as const,
        turnId: message.position === 'right' ? message.msg_id : message.turn_id,
        createdAt: message.created_at ?? 0,
      }))
    );
    const display = buildTurnDisclosureItems(grouped, { tailClosed: true });
    const disclosures = display.filter((entry) => entry.type === 'turn_disclosure');
    expect(disclosures).toHaveLength(1);
    expect(disclosures[0]?.turnId).toBe(rootTurnId);
    expect(disclosures[0]?.processItemIds).toEqual([rootTurnId, 'persisted-image-tool']);
    expect(disclosures[0]?.startAt).toBe(1500);
    expect(disclosures[0]?.endAt).toBe(3000);
    expect(display.map((entry) => entry.id)).toEqual([
      userId,
      `turn-disclosure-${rootTurnId}`,
      finalId,
    ]);
  });

  test('keeps one canonical old error in place instead of moving a live duplicate behind a later success', () => {
    const oldUser = baseMessage({
      id: 'db-old-user',
      msg_id: 'old-user',
      position: 'right',
      created_at: 100,
      content: { content: 'first request' },
    });
    const persistedOldError = baseMessage({
      id: 'db-old-error',
      msg_id: 'old-error-row',
      turn_id: messageId('old-turn'),
      type: 'tips',
      position: 'center',
      status: 'error',
      created_at: 200,
      content: { content: 'rate limited', type: 'error' },
    });
    const newUser = baseMessage({
      id: 'db-new-user',
      msg_id: 'new-user',
      position: 'right',
      created_at: 300,
      content: { content: 'hello' },
    });
    const successfulAnswer = baseMessage({
      id: 'db-success',
      msg_id: 'new-answer',
      created_at: 400,
      content: { content: 'Hello. What would you like to work on?' },
    });
    const staleLiveError = baseMessage({
      id: 'db-old-error',
      msg_id: 'old-error-row',
      turn_id: messageId('old-turn'),
      type: 'tips',
      position: 'center',
      status: 'error',
      created_at: 190,
      content: { content: 'rate limited', type: 'error' },
    });
    const persisted = [oldUser, persistedOldError, newUser, successfulAnswer];

    const merged = mergeFetchedMessagesForConversation(
      [oldUser, staleLiveError, newUser, successfulAnswer],
      persisted,
      oldUser.conversation_id
    );

    expect(merged.map((message) => message.id)).toEqual(persisted.map((message) => message.id));
    expect(merged.filter((message) => message.type === 'tips')).toHaveLength(1);
    expect(merged.at(-1)?.id).toBe('db-success');
  });

  test('dedupes the live and persisted error by canonical turn identity', () => {
    const liveError = baseMessage({
      id: 'renderer-error',
      msg_id: 'failed-turn',
      type: 'tips',
      position: 'center',
      status: 'error',
      content: { content: 'provider failed', type: 'error' },
    });
    const persistedError = baseMessage({
      id: 'db-error',
      msg_id: 'failed-turn',
      type: 'tips',
      position: 'center',
      status: 'error',
      content: { content: 'provider failed', type: 'error' },
    });

    const merged = mergeFetchedMessagesForConversation(
      [liveError],
      [persistedError],
      liveError.conversation_id
    );

    expect(merged).toEqual([persistedError]);
  });

  test('retains older persisted keyset pages in chronological position during a terminal refresh', () => {
    const olderUser = baseMessage({
      id: 'older-db-user',
      msg_id: 'older-user',
      position: 'right',
      created_at: 100,
      content: { content: 'older request' },
    });
    const olderAnswer = baseMessage({
      id: 'older-db-answer',
      msg_id: 'older-answer',
      created_at: 200,
      content: { content: 'older answer' },
    });
    const latestUser = baseMessage({
      id: 'latest-db-user',
      msg_id: 'latest-user',
      position: 'right',
      created_at: 300,
      content: { content: 'latest request' },
    });
    const latestAnswer = baseMessage({
      id: 'latest-db-answer',
      msg_id: 'latest-answer',
      created_at: 400,
      content: { content: 'latest answer' },
    });

    const merged = mergeFetchedMessagesForConversation(
      [olderUser, olderAnswer, latestUser, latestAnswer],
      [latestUser, latestAnswer],
      latestUser.conversation_id
    );

    expect(merged.map((message) => message.id)).toEqual([
      'older-db-user',
      'older-db-answer',
      'latest-db-user',
      'latest-db-answer',
    ]);
  });

  test('preserves a newer in-flight turn when an earlier authoritative refresh resolves late', () => {
    const completedUser = baseMessage({
      id: 'completed-user',
      msg_id: 'completed-user',
      position: 'right',
      created_at: 100,
      content: { content: 'completed request' },
    });
    const completedAnswer = baseMessage({
      id: 'completed-answer',
      msg_id: 'completed-answer',
      created_at: 200,
      content: { content: 'completed answer' },
    });
    const nextUser = baseMessage({
      id: 'next-live-user',
      msg_id: 'next-user',
      position: 'right',
      created_at: 300,
      content: { content: 'next request' },
      turn_id: messageId('next-turn'),
    });
    const nextPartialAnswer = baseMessage({
      id: 'next-live-answer',
      msg_id: 'next-answer',
      created_at: 350,
      status: 'work',
      content: { content: 'next partial answer' },
      turn_id: messageId('next-turn'),
    });

    const merged = mergeFetchedMessagesForConversation(
      [completedUser, completedAnswer, nextUser, nextPartialAnswer],
      [completedUser, completedAnswer],
      completedUser.conversation_id
    );

    expect(merged.map((message) => message.id)).toEqual([
      'completed-user',
      'completed-answer',
      'next-live-user',
      'next-live-answer',
    ]);
  });

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

    const merged = mergeFetchedMessagesForConversation([streamingThinking], [persistedThinking], parseConversationId('0190f5fe-7c00-7a00-8000-000000000004'));

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

    const merged = mergeFetchedMessagesForConversation([streamingThinking], [stalePersistedThinking], parseConversationId('0190f5fe-7c00-7a00-8000-000000000004'));

    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe(stalePersistedThinking.id);
    expect(merged[0].created_at).toBe(stalePersistedThinking.created_at);
    expect(merged[0].content).toEqual(streamingThinking.content);
  });

  test('does not restore a stale DB completed artifact over a live generic tool error', () => {
    const liveError = baseMessage({
      id: 'live-tool-error',
      msg_id: 'assistant-turn-artifact',
      type: 'tool_call',
      status: 'error',
      content: {
        call_id: 'image-call',
        name: 'Generate',
        args: {},
        status: 'error',
        artifacts: [],
      },
    } as any);
    const staleDbSuccess = baseMessage({
      id: 'persisted-tool-row',
      msg_id: 'assistant-turn-artifact',
      type: 'tool_call',
      status: 'finish',
      content: {
        call_id: 'image-call',
        name: 'Generate',
        args: {},
        status: 'completed',
        artifacts: [
          {
            id: '019b0000-0000-7000-8000-000000000002',
            kind: 'image',
            mime_type: 'image/png',
            path: '/workspace/stale.png',
            relative_path: 'nomifun-artifacts/stale.png',
            size_bytes: 10,
            sha256: 'a'.repeat(64),
          },
        ],
      },
    } as any);

    const merged = mergeFetchedMessagesForConversation(
      [liveError],
      [staleDbSuccess],
      liveError.conversation_id
    );

    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe('persisted-tool-row');
    expect((merged[0] as any).content.status).toBe('error');
    expect((merged[0] as any).content.artifacts).toEqual([]);
  });

  test('does not restore stale ACP artifact content over a live failed correction', () => {
    const liveFailure = baseMessage({
      id: 'live-acp-error',
      msg_id: 'assistant-turn-acp',
      type: 'acp_tool_call',
      status: 'error',
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'acp-image-call',
          status: 'failed',
          content: [],
        },
      },
    } as any);
    const staleDbSuccess = baseMessage({
      id: 'persisted-acp-row',
      msg_id: 'assistant-turn-acp',
      type: 'acp_tool_call',
      status: 'finish',
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'acp-image-call',
          status: 'completed',
          content: [
            {
              type: 'artifact',
              artifact: {
                id: '019b0000-0000-7000-8000-000000000002',
                kind: 'image',
                mime_type: 'image/png',
                path: '/workspace/stale-acp.png',
                relative_path: 'nomifun-artifacts/stale-acp.png',
                size_bytes: 10,
                sha256: 'b'.repeat(64),
              },
            },
          ],
        },
      },
    } as any);

    const merged = mergeFetchedMessagesForConversation(
      [liveFailure],
      [staleDbSuccess],
      liveFailure.conversation_id
    );

    expect((merged[0] as any).content.update.status).toBe('failed');
    expect((merged[0] as any).content.update.content).toEqual([]);
  });

  test('keys fetched tool lifecycles by call id so one turn can retain multiple tools', () => {
    const persistedCall = baseMessage({
      id: 'persisted-call-1',
      msg_id: 'assistant-multi-tool-turn',
      type: 'tool_call',
      content: { call_id: 'call-1', name: 'Read', args: {}, status: 'completed', artifacts: [] },
    } as any);
    const liveCallOne = baseMessage({
      id: 'live-call-1',
      msg_id: 'assistant-multi-tool-turn',
      type: 'tool_call',
      content: { call_id: 'call-1', name: 'Read', args: {}, status: 'completed', artifacts: [] },
    } as any);
    const liveCallTwo = baseMessage({
      id: 'live-call-2',
      msg_id: 'assistant-multi-tool-turn',
      type: 'tool_call',
      created_at: 1001,
      content: { call_id: 'call-2', name: 'Write', args: {}, status: 'running', artifacts: [] },
    } as any);

    const merged = mergeFetchedMessagesForConversation(
      [liveCallOne, liveCallTwo],
      [persistedCall],
      persistedCall.conversation_id
    );

    expect(merged).toHaveLength(2);
    expect(merged.map((message) => (message as any).content.call_id)).toEqual(['call-1', 'call-2']);
  });
});

describe('composeMessageForTest', () => {
  test('keeps the first ACP tool envelope stable when a terminal frame arrives late', () => {
    const turnId = messageId('stable-turn');
    const running = baseMessage({
      id: 'first-live-tool-id',
      msg_id: 'stable-turn',
      turn_id: turnId,
      type: 'acp_tool_call',
      created_at: 2000,
      content: {
        session_id: 'session-stable',
        update: {
          session_update: 'tool_call_update',
          tool_call_id: 'stable-call',
          status: 'in_progress',
        },
      },
    } as any);
    const completed = baseMessage({
      id: 'late-terminal-tool-id',
      msg_id: 'stable-turn',
      turn_id: turnId,
      type: 'acp_tool_call',
      created_at: 5000,
      content: {
        session_id: 'session-stable',
        update: {
          session_update: 'tool_call_update',
          tool_call_id: 'stable-call',
          status: 'completed',
        },
      },
    } as any);

    const merged = composeMessageForTest(completed, [running]);

    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe('first-live-tool-id');
    expect(merged[0].created_at).toBe(2000);
    expect(merged[0].turn_id).toBe(turnId);
    expect((merged[0] as any).content.update.status).toBe('completed');
  });

  test('backfills the root turn id when a thinking completion merges into an early unowned row', () => {
    const rootTurnId = messageId('thinking-root');
    const running = baseMessage({
      id: 'first-thinking-id',
      msg_id: 'thinking-segment',
      type: 'thinking',
      created_at: 1000,
      content: { content: '正在分析请求', status: 'thinking' },
    });
    const completed = baseMessage({
      id: 'late-thinking-done-id',
      msg_id: 'thinking-segment',
      turn_id: rootTurnId,
      type: 'thinking',
      created_at: 5000,
      content: { content: '', status: 'done', duration: 4000 },
    });

    const merged = composeMessageForTest(completed, [running]);

    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe('first-thinking-id');
    expect(merged[0].created_at).toBe(1000);
    expect(merged[0].turn_id).toBe(rootTurnId);
    expect(merged[0].content).toMatchObject({ status: 'done', duration: 4000 });
  });

  test('keeps terminal tips separate from successful text sharing the same stream msg_id', () => {
    const text = baseMessage({
      id: 'successful-text',
      msg_id: 'shared-stream-segment',
      type: 'text',
      content: { content: 'Task completed successfully.' },
    });
    const error = baseMessage({
      id: 'terminal-error',
      msg_id: 'shared-stream-segment',
      type: 'tips',
      position: 'center',
      status: 'error',
      content: { content: 'provider failed', type: 'error' },
    });

    const errorAfterText = composeMessageForTest(error, [text]);
    const textAfterError = composeMessageForTest(text, [error]);

    expect(errorAfterText.map((message) => message.id)).toEqual(['successful-text', 'terminal-error']);
    expect(textAfterError.map((message) => message.id)).toEqual(['terminal-error', 'successful-text']);
  });

  test('applies a hidden terminal update to the matching tool in the same turn', () => {
    const running = baseMessage({
      id: 'turn-1:tool:call-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      content: { call_id: 'call-1', name: 'update_plan', args: {}, status: 'running' },
    } as any);
    const completed = baseMessage({
      id: 'turn-1:tool:call-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      hidden: true,
      content: { call_id: 'call-1', name: 'update_plan', args: {}, status: 'completed' },
    } as any);

    const merged = composeMessageForTest(completed, [running]);

    expect(merged).toHaveLength(1);
    expect(merged[0].hidden).toBe(true);
    expect((merged[0] as any).content.status).toBe('completed');
  });

  test('applies a terminal error correction over completed artifact delivery', () => {
    const completed = baseMessage({
      id: 'turn-1:tool:artifact-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      content: {
        call_id: 'artifact-1',
        name: 'Generate',
        args: {},
        status: 'completed',
        artifacts: [
          {
            id: '019b0000-0000-7000-8000-000000000002',
            kind: 'image',
            mime_type: 'image/png',
            path: '/workspace/old.png',
            relative_path: 'nomifun-artifacts/old.png',
            size_bytes: 10,
            sha256: 'a'.repeat(64),
          },
        ],
      },
    } as any);
    const correction = baseMessage({
      id: 'turn-1:tool:artifact-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      content: {
        call_id: 'artifact-1',
        name: 'Generate',
        args: {},
        status: 'error',
        artifacts: [],
      },
    } as any);

    const merged = composeMessageForTest(correction, [completed]);

    expect(merged).toHaveLength(1);
    expect((merged[0] as any).content.status).toBe('error');
    expect((merged[0] as any).content.artifacts).toEqual([]);
  });

  test('keeps generic tool error absorbing against late completed artifact delivery', () => {
    const failed = baseMessage({
      id: 'turn-1:tool:artifact-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      content: { call_id: 'artifact-1', name: 'Generate', args: {}, status: 'error', artifacts: [] },
    } as any);
    const lateCompleted = baseMessage({
      id: 'turn-1:tool:artifact-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      content: {
        call_id: 'artifact-1',
        name: 'Generate',
        args: {},
        status: 'completed',
        artifacts: [
          {
            id: '019b0000-0000-7000-8000-000000000002',
            kind: 'image',
            mime_type: 'image/png',
            path: '/workspace/stale.png',
            relative_path: 'nomifun-artifacts/stale.png',
            size_bytes: 10,
            sha256: 'b'.repeat(64),
          },
        ],
      },
    } as any);

    const merged = composeMessageForTest(lateCompleted, [failed]);

    expect((merged[0] as any).content.status).toBe('error');
    expect((merged[0] as any).content.artifacts).toEqual([]);
  });

  test('indexed ACP failure correction removes inherited completed artifact content', () => {
    const completed = baseMessage({
      id: 'turn-1:acp:artifact-1',
      msg_id: 'turn-1',
      type: 'acp_tool_call',
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'acp-artifact-1',
          status: 'completed',
          content: [
            {
              type: 'resource_link',
              name: 'report.pdf',
              uri: 'https://example.invalid/report.pdf',
            },
          ],
        },
      },
    } as any);
    const failed = baseMessage({
      id: 'turn-1:acp:artifact-1',
      msg_id: 'turn-1',
      type: 'acp_tool_call',
      status: 'error',
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'acp-artifact-1',
          status: 'failed',
        },
      },
    } as any);

    const merged = composeMessageForTest(failed, [completed]);

    expect((merged[0] as any).content.update.status).toBe('failed');
    expect((merged[0] as any).content.update.content).toEqual([]);
  });

  test('does not merge reused provider call ids across turns', () => {
    const firstTurn = baseMessage({
      id: 'turn-1:tool:call-1',
      msg_id: 'turn-1',
      type: 'tool_call',
      content: { call_id: 'call-1', name: 'Read', args: {}, status: 'completed' },
    } as any);
    const secondTurn = baseMessage({
      id: 'turn-2:tool:call-1',
      msg_id: 'turn-2',
      type: 'tool_call',
      content: { call_id: 'call-1', name: 'Read', args: {}, status: 'running' },
    } as any);

    const merged = composeMessageForTest(secondTurn, [firstTurn]);

    expect(merged).toHaveLength(2);
    expect(merged.map((message) => message.msg_id)).toEqual([messageId('turn-1'), messageId('turn-2')]);
  });

  test('replaces the current plan by session_id even when the incoming msg_id changes', () => {
    const oldPlan = baseMessage({
      id: 'turn-1:plan:update_plan',
      msg_id: 'turn-1:plan:update_plan',
      type: 'plan',
      content: {
        session_id: 'update_plan',
        entries: [
          { content: 'Inspect', status: 'completed' },
          { content: 'Implement', status: 'in_progress' },
          { content: 'Verify', status: 'pending' },
        ],
      },
    });
    const text = baseMessage({
      id: 'assistant-text',
      msg_id: 'assistant-text',
      type: 'text',
      content: { content: 'Working...' },
    });
    const updatedPlan = baseMessage({
      id: 'turn-2:plan:update_plan',
      msg_id: 'turn-2:plan:update_plan',
      type: 'plan',
      content: {
        session_id: 'update_plan',
        entries: [
          { content: 'Inspect', status: 'completed' },
          { content: 'Implement', status: 'completed' },
          { content: 'Verify', status: 'completed' },
        ],
      },
    });

    const merged = composeMessageForTest(updatedPlan, [oldPlan, text]);

    expect(merged).toHaveLength(2);
    expect(merged[0]).toEqual(text);
    expect(merged[1]).toEqual(updatedPlan);
  });

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

  test('merges knowledge writeback state into the existing assistant text message', () => {
    const text = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: { content: 'Final answer is already visible.' },
    });
    const writeback = baseMessage({
      id: 'writeback-event',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: {
        content: '',
        knowledge_writeback: {
          status: 'writing',
          attempt_id: 'attempt-1',
          retryable: false,
        },
      },
    });

    const merged = composeMessageForTest(writeback, [text]);

    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe('assistant-turn-1');
    expect(merged[0].type).toBe('text');
    if (merged[0].type !== 'text') throw new Error('expected text message');
    expect(merged[0].content.content).toBe('Final answer is already visible.');
    expect(merged[0].content.knowledge_writeback?.status).toBe('writing');
  });

  test('keeps knowledge writeback visible when its event arrives before the assistant text', () => {
    const writeback = baseMessage({
      id: 'writeback-event',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: {
        content: '',
        knowledge_writeback: {
          status: 'writing',
          attempt_id: 'attempt-1',
        },
      },
    });

    const pending = composeMessageForTest(writeback, []);

    expect(pending).toHaveLength(1);
    expect(pending[0].type).toBe('text');
    if (pending[0].type !== 'text') throw new Error('expected text message');
    expect(pending[0].content.content).toBe('');
    expect(pending[0].content.knowledge_writeback?.status).toBe('writing');
  });

  test('merges assistant text into an early knowledge writeback process row', () => {
    const writeback = baseMessage({
      id: 'writeback-event',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: {
        content: '',
        knowledge_writeback: {
          status: 'writing',
          attempt_id: 'attempt-1',
          updated_at: 10,
        },
      },
    });
    const other = baseMessage({
      id: 'other-turn',
      msg_id: 'other-turn',
      type: 'text',
      content: { content: 'Another visible message.' },
    });
    const text = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: { content: 'Final answer arrived after the writeback event.' },
    });

    const pending = composeMessageForTest(writeback, [other]);
    const merged = composeMessageForTest(text, pending);

    expect(merged).toHaveLength(2);
    expect(merged[1].id).toBe('writeback-event');
    expect(merged[1].type).toBe('text');
    if (merged[1].type !== 'text') throw new Error('expected text message');
    expect(merged[1].content.content).toBe('Final answer arrived after the writeback event.');
    expect(merged[1].content.knowledge_writeback?.status).toBe('writing');
  });
});

describe('normalizeDbMessage', () => {
  const persistedArtifact = {
    id: '019b0000-0000-7000-8000-000000000002',
    kind: 'image',
    mime_type: 'image/png',
    path: '/workspace/image.png',
    relative_path: 'nomifun-artifacts/image.png',
    size_bytes: 10,
    sha256: 'c'.repeat(64),
  };

  test('keeps the owning turn identity supplied by the transport boundary', () => {
    const turnId = messageId('failed-turn');
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'persisted-terminal-error',
        msg_id: 'terminal-error-row',
        turn_id: turnId,
        type: 'tips',
        status: 'error',
        content: {
          content: 'provider failed',
          type: 'error',
          error: { message: 'provider failed', code: 'USER_LLM_PROVIDER_RATE_LIMITED' },
        } as any,
      })
    );

    expect(normalized.type).toBe('tips');
    expect(normalized.turn_id).toBe(turnId);
  });

  test('keeps persisted turn identity for generic tools, ACP tools, and text', () => {
    const genericTurnId = messageId('generic-turn');
    const acpTurnId = messageId('acp-turn');
    const textTurnId = messageId('text-turn');

    const generic = normalizeDbMessage(
      baseMessage({
        id: 'persisted-generic-tool',
        msg_id: 'generic-tool-row',
        turn_id: genericTurnId,
        type: 'tool_call',
        content: {
          call_id: 'generic-call',
          name: 'Generate',
          status: 'running',
        } as any,
      })
    );
    const acp = normalizeDbMessage(
      baseMessage({
        id: 'persisted-acp-tool',
        msg_id: 'acp-tool-row',
        turn_id: acpTurnId,
        type: 'acp_tool_call',
        content: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'acp-call',
            status: 'in_progress',
          },
        } as any,
      })
    );
    const text = normalizeDbMessage(
      baseMessage({
        id: 'persisted-text',
        msg_id: 'text-row',
        turn_id: textTurnId,
        type: 'text',
        content: {
          content: 'Generated successfully.',
        } as any,
      })
    );

    expect(generic.turn_id).toBe(genericTurnId);
    expect(acp.turn_id).toBe(acpTurnId);
    expect(text.turn_id).toBe(textTurnId);
  });

  test('normalizes a transport-decoded persisted content object', () => {
    const turnId = messageId('object-text-turn');
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'decoded-text-row',
        msg_id: 'decoded-text-row',
        turn_id: turnId,
        type: 'text',
        content: { content: 'Already decoded.' } as any,
      })
    );

    expect(normalized.type).toBe('text');
    expect(normalized.turn_id).toBe(turnId);
    expect(normalized.content).toEqual({ content: 'Already decoded.' });
  });

  test('keeps validated decoded cron and Agent metadata', () => {
    const turnId = messageId('metadata-turn');
    const senderConversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000007');
    const cronJobId = parseCronJobId('019b0000-0000-7000-8000-000000000001');
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'decoded-metadata-row',
        msg_id: 'decoded-metadata-row',
        turn_id: turnId,
        type: 'text',
        content: {
          content: 'Automated Agent result.',
          replace: true,
          cronMeta: {
            source: 'cron',
            cron_job_id: cronJobId,
            cron_job_name: 'Daily report',
            triggered_at: 1234,
          },
          agentMessage: true,
          senderName: 'Researcher',
          senderAgentType: 'codex',
          senderConversationId,
        } as any,
      })
    );

    expect(normalized.type).toBe('text');
    expect(normalized.turn_id).toBe(turnId);
    expect(normalized.content).toEqual({
      content: 'Automated Agent result.',
      replace: true,
      cronMeta: {
        source: 'cron',
        cron_job_id: cronJobId,
        cron_job_name: 'Daily report',
        triggered_at: 1234,
      },
      agentMessage: true,
      senderName: 'Researcher',
      senderAgentType: 'codex',
      senderConversationId,
    });
  });

  test('uses only the top-level turn identity and never promotes nested content metadata', () => {
    const rowTurnId = messageId('row-turn');
    const nestedTurnId = messageId('content-turn');
    const rowWins = normalizeDbMessage(
      baseMessage({
        id: 'row-turn-wins',
        msg_id: 'row-turn-wins',
        turn_id: rowTurnId,
        type: 'tips',
        content: {
          content: 'row identity is authoritative',
          type: 'warning',
          turn_id: nestedTurnId,
        } as any,
      })
    );
    const nestedOnly = normalizeDbMessage(
      baseMessage({
        id: 'nested-only-turn',
        msg_id: 'nested-only-turn',
        type: 'tips',
        content: {
          content: 'content identity is valid',
          type: 'warning',
          turn_id: nestedTurnId,
        } as any,
      })
    );

    expect(rowWins.turn_id).toBe(rowTurnId);
    expect(rowWins.content).toEqual({
      content: 'row identity is authoritative',
      type: 'warning',
    });
    expect(nestedOnly.turn_id).toBeUndefined();
  });

  test('does not promote nested turn metadata from renderer content', () => {
    const malformed = 'turn-from-another-protocol';
    const normalized = [
      normalizeDbMessage(
        baseMessage({
          id: 'invalid-tips-turn',
          type: 'tips',
          content: { content: 'warning', type: 'warning', turn_id: malformed } as any,
        })
      ),
      normalizeDbMessage(
        baseMessage({
          id: 'invalid-generic-turn',
          type: 'tool_call',
          content: {
            call_id: 'invalid-generic-call',
            name: 'Generate',
            status: 'running',
            turn_id: malformed,
          } as any,
        })
      ),
      normalizeDbMessage(
        baseMessage({
          id: 'invalid-acp-turn',
          type: 'acp_tool_call',
          content: {
            session_id: 'session-invalid',
            turn_id: malformed,
            update: {
              sessionUpdate: 'tool_call_update',
              tool_call_id: 'invalid-acp-call',
              status: 'in_progress',
            },
          } as any,
        })
      ),
      normalizeDbMessage(
        baseMessage({
          id: 'invalid-text-turn',
          type: 'text',
          content: { content: 'answer', turn_id: malformed } as any,
        })
      ),
    ];

    expect(normalized.map((message) => message.turn_id)).toEqual([
      undefined,
      undefined,
      undefined,
      undefined,
    ]);
  });

  test('keeps a delayed persisted tool attached to its old turn after a newer user request', () => {
    const oldTurnId = messageId('old-tool-turn');
    const newTurnId = messageId('new-user-turn');
    const delayedTool = normalizeDbMessage(
      baseMessage({
        id: 'delayed-old-tool',
        msg_id: 'delayed-old-tool-row',
        turn_id: oldTurnId,
        type: 'tool_call',
        created_at: 300,
        content: {
          call_id: 'delayed-old-call',
          name: 'Generate',
          status: 'error',
        } as any,
      })
    );

    const grouped = assignTurnIdsFromUserRequests([
      { id: 'old-user', role: 'user', turnId: oldTurnId, createdAt: 100 },
      { id: 'new-user', role: 'user', turnId: newTurnId, createdAt: 200 },
      { id: delayedTool.id, role: 'process', turnId: delayedTool.turn_id, createdAt: 300 },
      { id: 'new-unowned-tool', role: 'process', createdAt: 400 },
    ]);

    expect(grouped.map((item) => item.turnId)).toEqual([
      oldTurnId,
      newTurnId,
      oldTurnId,
      newTurnId,
    ]);
  });

  test('row-level generic error removes stale completed artifact receipts from history', () => {
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'failed-tool-row',
        msg_id: 'assistant-failed-tool',
        type: 'tool_call',
        status: 'error',
        content: {
          call_id: 'failed-image',
          name: 'Generate',
          status: 'completed',
          artifacts: [persistedArtifact],
        } as any,
      })
    );

    expect(normalized.type).toBe('tool_call');
    if (normalized.type !== 'tool_call') throw new Error('expected generic tool call');
    expect(normalized.content.status).toBe('error');
    expect(normalized.content.artifacts).toEqual([]);
  });

  test('row-level ACP error removes stale completed artifact receipts from history', () => {
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'failed-acp-row',
        msg_id: 'assistant-failed-acp',
        type: 'acp_tool_call',
        status: 'error',
        content: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'failed-acp-image',
            status: 'completed',
            content: [{ type: 'artifact', artifact: persistedArtifact }],
          },
        } as any,
      })
    );

    expect(normalized.type).toBe('acp_tool_call');
    if (normalized.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(normalized.content.update.status).toBe('failed');
    expect(normalized.content.update.content).toEqual([]);
  });

  test('history hydration rejects an entire completed receipt batch when one member is malformed', () => {
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'mixed-tool-row',
        msg_id: 'assistant-mixed-tool',
        type: 'tool_call',
        status: 'finish',
        content: {
          call_id: 'mixed-image',
          name: 'Generate',
          status: 'completed',
          artifact_delivery_committed: true,
          artifacts: [persistedArtifact, { ...persistedArtifact, id: 'bad', sha256: 'invalid' }],
        },
      } as any)
    );

    expect(normalized.type).toBe('tool_call');
    if (normalized.type !== 'tool_call') throw new Error('expected generic tool call');
    expect(normalized.content.status).toBe('error');
    expect(normalized.content.artifacts).toEqual([]);
  });

  test('history hydration fails a completed ACP batch containing an unsafe resource URI', () => {
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'unsafe-acp-row',
        msg_id: 'assistant-unsafe-acp',
        type: 'acp_tool_call',
        status: 'finish',
        content: {
          session_id: 'session-unsafe',
          artifact_delivery_committed: true,
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'unsafe-resource',
            status: 'completed',
            content: [
              { type: 'resource_link', name: 'unsafe', uri: 'javascript:alert(1)' },
              { type: 'artifact', artifact: persistedArtifact },
            ],
          },
        },
      } as any)
    );

    expect(normalized.type).toBe('acp_tool_call');
    if (normalized.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(normalized.content.update.status).toBe('failed');
    expect(
      normalized.content.update.content?.some(
        (item) => item.type === 'artifact' || item.type === 'resource_link'
      )
    ).toBe(false);
    expect(
      normalized.content.update.content?.some(
        (item) =>
          item.type === 'artifact_error' &&
          item.message === 'Invalid or unsafe resource link'
      )
    ).toBe(true);
  });

  test('history exposes generic receipts only after the enclosing turn commit marker', () => {
    const hydrate = (artifact_delivery_committed?: boolean) =>
      normalizeDbMessage(
        baseMessage({
          id: artifact_delivery_committed ? 'committed-tool-row' : 'legacy-tool-row',
          msg_id: 'assistant-committed-tool',
          type: 'tool_call',
          status: 'finish',
          content: {
            call_id: 'committed-image',
            name: 'Generate',
            status: 'completed',
            ...(artifact_delivery_committed === undefined ? {} : { artifact_delivery_committed }),
            artifacts: [persistedArtifact],
          },
        } as any)
      );

    const legacy = hydrate();
    if (legacy.type !== 'tool_call') throw new Error('expected generic tool call');
    expect(legacy.content.status).toBe('error');
    expect(legacy.content.artifacts).toEqual([]);

    const committed = hydrate(true);
    if (committed.type !== 'tool_call') throw new Error('expected generic tool call');
    expect(committed.content.status).toBe('completed');
    expect(committed.content.artifacts).toEqual([persistedArtifact]);
  });

  test('history exposes ACP deliveries only after the enclosing turn commit marker', () => {
    const hydrate = (artifact_delivery_committed?: boolean) =>
      normalizeDbMessage(
        baseMessage({
          id: artifact_delivery_committed ? 'committed-acp-row' : 'legacy-acp-row',
          msg_id: 'assistant-committed-acp',
          type: 'acp_tool_call',
          status: 'finish',
          content: {
            session_id: 'session-committed',
            ...(artifact_delivery_committed === undefined ? {} : { artifact_delivery_committed }),
            update: {
              sessionUpdate: 'tool_call_update',
              tool_call_id: 'committed-acp-image',
              status: 'completed',
              content: [{ type: 'artifact', artifact: persistedArtifact }],
            },
          },
        } as any)
      );

    const legacy = hydrate();
    if (legacy.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(legacy.content.update.status).toBe('failed');
    expect(
      legacy.content.update.content?.some(
        (item) => item.type === 'artifact' || item.type === 'resource_link'
      )
    ).toBe(false);

    const committed = hydrate(true);
    if (committed.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(committed.content.update.status).toBe('completed');
    expect(
      committed.content.update.content?.some(
        (item) => item.type === 'artifact' && item.artifact.id === persistedArtifact.id
      )
    ).toBe(true);
  });

  test('history downgrades receipt-less tool-group image success before process rendering', () => {
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'receiptless-image-group-row',
        msg_id: 'assistant-receiptless-image-group',
        type: 'tool_group',
        status: 'finish',
        content: [
          {
            call_id: 'receiptless-image-group',
            name: 'ImageGeneration',
            description: 'generated',
            status: 'Success',
            result_display: {
              img_url: '/workspace/old.png',
              relative_path: 'old.png',
            },
          },
        ] as any,
      })
    );

    expect(normalized.type).toBe('tool_group');
    if (normalized.type !== 'tool_group') throw new Error('expected tool group');
    expect(normalized.status).toBe('error');
    expect(normalized.content[0].status).toBe('Error');
    expect(normalized.content[0].result_display).toBeUndefined();
  });

  test('preserves persisted knowledge writeback state from decoded text content', () => {
    const normalized = normalizeDbMessage(
      baseMessage({
        id: 'assistant-turn-1',
        msg_id: 'assistant-turn-1',
        type: 'text',
        content: {
          content: 'Final answer.',
          knowledge_writeback: {
            status: 'written',
            updated_at: 20,
            written: [
              {
                kb_id: '0190f5fe-7c00-7a00-8000-000000000001',
                rel_path: '_inbox/1/patterns/final.md',
                staged: true,
              },
            ],
          },
        } as any,
      })
    );

    expect(normalized.type).toBe('text');
    if (normalized.type !== 'text') throw new Error('expected text message');
    expect(normalized.content.content).toBe('Final answer.');
    expect(normalized.content.knowledge_writeback?.status).toBe('written');
    expect(normalized.content.knowledge_writeback?.written?.[0]?.rel_path).toBe('_inbox/1/patterns/final.md');
  });

  test('keeps newer persisted writeback state while preserving longer streaming text', () => {
    const streaming = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: {
        content: 'Final answer is already visible with the complete streamed text.',
        knowledge_writeback: {
          status: 'writing',
          attempt_id: 'attempt-1',
          updated_at: 10,
        },
      },
    });
    const persisted = baseMessage({
      id: 'assistant-turn-1',
      msg_id: 'assistant-turn-1',
      type: 'text',
      content: {
        content: 'Final answer.',
        knowledge_writeback: {
          status: 'written',
          attempt_id: 'attempt-1',
          updated_at: 20,
        },
      },
    });

    const merged = mergeFetchedMessagesForConversation([streaming], [persisted], parseConversationId('0190f5fe-7c00-7a00-8000-000000000004'));

    expect(merged).toHaveLength(1);
    expect(merged[0].type).toBe('text');
    if (merged[0].type !== 'text') throw new Error('expected text message');
    expect(merged[0].content.content).toBe('Final answer is already visible with the complete streamed text.');
    expect(merged[0].content.knowledge_writeback?.status).toBe('written');
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

  test('stringifies malformed thinking stream chunks instead of throwing', () => {
    let result = '';
    let error: unknown;
    try {
      result = mergeThinkingStreamContent({ existing: true } as any, { incoming: true } as any);
    } catch (caught) {
      error = caught;
    }
    expect(error).toBeUndefined();
    expect(result.includes('"incoming": true')).toBe(true);
  });
});
