/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  parseCompanionId,
  parseConversationId,
  parseKnowledgeBaseId,
  parseMessageId,
} from '@/common/types/ids';
import {
  composeMessage,
  joinPath,
  transformKnowledgeWritebackEvent,
  transformMessage,
  transformUserCreatedEvent,
} from './chatLib';

const MESSAGE_ID = parseMessageId('019b0000-0000-7000-8000-000000000001');
const SECOND_MESSAGE_ID = parseMessageId('019b0000-0000-7000-8000-000000000002');
const COMPANION_ID = parseCompanionId('019b0000-0000-7000-8000-000000000001');

const baseWire = (overrides: Record<string, unknown>) =>
  ({
    msg_id: MESSAGE_ID,
    conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000001'),
    ...overrides,
  }) as any;

describe('joinPath compatibility export', () => {
  test('preserves UNC and URI prefixes', () => {
    expect(joinPath('//server/share/project', '../cat.png')).toBe('//server/share/cat.png');
    expect(joinPath('https://example.com/assets', 'cat.png')).toBe('https://example.com/assets/cat.png');
  });
});

describe('transformMessage runtime field normalization', () => {
  test('ACP partial updates preserve prior title, kind, and status instead of injecting defaults', () => {
    const initial = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call',
            tool_call_id: 'tool-1',
            status: 'in_progress',
            title: 'Generate image',
            kind: 'execute',
          },
        },
      })
    )!;
    const partial = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-1',
            rawOutput: { progress: 50 },
          },
        },
      })
    )!;

    expect(partial.type).toBe('acp_tool_call');
    if (partial.type !== 'acp_tool_call' || initial.type !== 'acp_tool_call') {
      throw new Error('expected ACP tool messages');
    }
    expect(partial.content.update.status).toBeUndefined();
    expect(partial.content.update.title).toBeUndefined();
    expect(partial.content.update.kind).toBeUndefined();

    const merged = composeMessage(partial, [initial]);
    expect(merged).toHaveLength(1);
    const update = (merged[0] as typeof initial).content.update;
    expect(update.status).toBe('in_progress');
    expect(update.title).toBe('Generate image');
    expect(update.kind).toBe('execute');
    expect(update.rawOutput).toEqual({ progress: 50 });
  });

  test('ACP failed status is absorbing across late completed updates', () => {
    const failed = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-1',
            status: 'failed',
            title: 'Generate image',
            kind: 'execute',
            content: [{ type: 'artifact_error', message: 'invalid image' }],
          },
        },
      })
    )!;
    const lateCompleted = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-1',
            status: 'completed',
          },
        },
      })
    )!;

    const merged = composeMessage(lateCompleted, [failed]);
    expect(merged).toHaveLength(1);
    const mergedMessage = merged[0];
    if (mergedMessage.type !== 'acp_tool_call') throw new Error('expected ACP tool message');
    expect(mergedMessage.content.update.status).toBe('failed');
  });

  test('ACP completed status is not downgraded by a replayed in-progress frame', () => {
    const completed = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-completed',
            status: 'completed',
            title: 'Generate image',
          },
        },
      })
    )!;
    const replayedProgress = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-completed',
            status: 'in_progress',
          },
        },
      })
    )!;

    const merged = composeMessage(replayedProgress, [completed]);
    expect(merged).toHaveLength(1);
    const mergedMessage = merged[0];
    if (mergedMessage.type !== 'acp_tool_call') throw new Error('expected ACP tool message');
    expect(mergedMessage.content.update.status).toBe('completed');
    expect(mergedMessage.content.update.title).toBe('Generate image');
  });

  test('ACP failed correction removes artifact content inherited from completed state', () => {
    const completed = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-artifact',
            status: 'completed',
            content: [
              {
                type: 'artifact',
                artifact: {
                  id: '019b0000-0000-7000-8000-000000000002',
                  kind: 'image',
                  mime_type: 'image/png',
                  path: '/workspace/old.png',
                  relative_path: 'nomifun-artifacts/old.png',
                  size_bytes: 10,
                  sha256: 'd'.repeat(64),
                },
              },
            ],
          },
        },
      })
    )!;
    const failed = transformMessage(
      baseWire({
        type: 'acp_tool_call',
        data: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-artifact',
            status: 'failed',
          },
        },
      })
    )!;

    const merged = composeMessage(failed, [completed]);
    const message = merged[0];
    if (message.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(message.content.update.status).toBe('failed');
    expect(message.content.update.content).toEqual([]);
  });

  test('generic tool failure is absorbing across a late completed artifact frame', () => {
    const failed = transformMessage(
      baseWire({
        type: 'tool_call',
        data: { call_id: 'tool-1', name: 'Generate', status: 'error', output: 'failed' },
      })
    )!;
    const lateCompleted = transformMessage(
      baseWire({
        type: 'tool_call',
        data: {
          call_id: 'tool-1',
          name: 'Generate',
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
      })
    )!;

    const merged = composeMessage(lateCompleted, [failed]);
    const message = merged[0];
    if (message.type !== 'tool_call') throw new Error('expected tool call');
    expect(message.content.status).toBe('error');
    expect(message.content.artifacts).toEqual([]);
  });

  test('generic tool error correction retracts an earlier completed artifact frame', () => {
    const completed = transformMessage(
      baseWire({
        type: 'tool_call',
        data: {
          call_id: 'tool-corrected',
          name: 'Generate',
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
      })
    )!;
    const correction = transformMessage(
      baseWire({
        type: 'tool_call',
        data: {
          call_id: 'tool-corrected',
          name: 'Generate',
          status: 'error',
          output: 'enclosing turn failed',
          artifacts: [],
        },
      })
    )!;

    const merged = composeMessage(correction, [completed]);
    const message = merged[0];
    if (message.type !== 'tool_call') throw new Error('expected tool call');
    expect(message.content.status).toBe('error');
    expect(message.content.artifacts).toEqual([]);
  });

  test('only completed tool calls retain structurally valid durable artifact receipts', () => {
    const artifact = {
      id: '019b0000-0000-7000-8000-000000000002',
      kind: 'image',
      mime_type: 'image/png',
      path: '/workspace/nomifun-artifacts/image.png',
      relative_path: 'nomifun-artifacts/image.png',
      size_bytes: 10,
      sha256: 'a'.repeat(64),
    };
    const transform = (status: 'running' | 'completed' | 'error', value: Record<string, unknown>) =>
      transformMessage(
        baseWire({
          type: 'tool_call',
          data: { call_id: `tool-${status}`, name: 'Generate', status, artifacts: [value] },
        })
      );

    const completed = transform('completed', artifact);
    if (completed?.type !== 'tool_call') throw new Error('expected completed tool call');
    expect(completed.content.artifacts).toEqual([artifact]);

    for (const status of ['running', 'error'] as const) {
      const message = transform(status, artifact);
      if (message?.type !== 'tool_call') throw new Error('expected tool call');
      expect(message.content.artifacts).toEqual([]);
    }

    for (const malformed of [
      { ...artifact, sha256: 'not-a-sha' },
      { ...artifact, size_bytes: 0 },
      { ...artifact, path: 'relative/image.png' },
      { ...artifact, relative_path: '../old.png' },
    ]) {
      const message = transform('completed', malformed);
      if (message?.type !== 'tool_call') throw new Error('expected tool call');
      expect(message.content.artifacts).toEqual([]);
    }
  });

  test('ACP artifact content requires completed status and a valid receipt', () => {
    const artifact = {
      id: '019b0000-0000-7000-8000-000000000002',
      kind: 'image',
      mime_type: 'image/png',
      path: String.raw`C:\workspace\nomifun-artifacts\image.png`,
      relative_path: 'nomifun-artifacts/image.png',
      size_bytes: 10,
      sha256: 'b'.repeat(64),
    };
    const transform = (status: 'in_progress' | 'completed' | 'failed', value: Record<string, unknown>) =>
      transformMessage(
        baseWire({
          type: 'acp_tool_call',
          data: {
            session_id: 'session-1',
            update: {
              sessionUpdate: 'tool_call_update',
              tool_call_id: `acp-${status}`,
              status,
              content: [{ type: 'artifact', artifact: value }],
            },
          },
        })
      );

    const completed = transform('completed', artifact);
    if (completed?.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(completed.content.update.content?.[0]).toMatchObject({ type: 'artifact', artifact });

    for (const status of ['in_progress', 'failed'] as const) {
      const message = transform(status, artifact);
      if (message?.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
      expect(message.content.update.content).toEqual([]);
    }

    const malformed = transform('completed', { ...artifact, relative_path: '../old.png' });
    if (malformed?.type !== 'acp_tool_call') throw new Error('expected ACP tool call');
    expect(malformed.content.update.content).toEqual([
      { type: 'artifact_error', message: 'Invalid or incomplete artifact receipt' },
    ]);
  });

  test('composeMessage keeps reused tool call ids isolated by turn', () => {
    const first = transformMessage(baseWire({
      msg_id: MESSAGE_ID,
      type: 'tool_call',
      data: { call_id: 'call-1', name: 'Read', status: 'completed' },
    }))!;
    const second = transformMessage(baseWire({
      msg_id: SECOND_MESSAGE_ID,
      type: 'tool_call',
      data: { call_id: 'call-1', name: 'Read', status: 'running' },
    }))!;

    expect(composeMessage(second, [first])).toHaveLength(2);
  });

  test('legacy tool-group Error is absorbing across a late image Success frame', () => {
    const wire = (status: 'Error' | 'Success', result_display?: Record<string, string>) =>
      transformMessage(
        baseWire({
          type: 'tool_group',
          data: [
            {
              call_id: 'legacy-image',
              name: 'ImageGeneration',
              description: 'generate',
              status,
              result_display,
            },
          ],
        })
      )!;
    const failed = wire('Error');
    const lateSuccess = wire('Success', { img_url: '/workspace/old.png', relative_path: 'old.png' });
    const merged = composeMessage(lateSuccess, [failed]);
    const message = merged[0];
    if (message.type !== 'tool_group') throw new Error('expected tool group');
    expect(message.content[0].status).toBe('Error');
    expect(message.content[0].result_display).toBeUndefined();
  });

  test('legacy tool-group image Success is downgraded at message admission without receipt authority', () => {
    const message = transformMessage(
      baseWire({
        type: 'tool_group',
        data: [
          {
            call_id: 'legacy-unverified-image',
            name: 'ImageGeneration',
            description: 'generated',
            status: 'Success',
            result_display: {
              img_url: '/workspace/old.png',
              relative_path: 'old.png',
            },
          },
        ],
      })
    );

    if (message?.type !== 'tool_group') throw new Error('expected tool group');
    expect(message.content[0].status).toBe('Error');
    expect(message.content[0].result_display).toBeUndefined();
    expect(message.content[0].description.includes('committed artifact receipt')).toBe(true);
  });

  test('legacy tool-group terminal Success cannot inherit a provisional image path', () => {
    const wire = (status: 'Executing' | 'Success', result_display?: Record<string, string>) =>
      transformMessage(
        baseWire({
          type: 'tool_group',
          data: [
            {
              call_id: 'legacy-image',
              name: 'ImageGeneration',
              description: 'generate',
              status,
              ...(result_display ? { result_display } : {}),
            },
          ],
        })
      )!;
    const progress = wire('Executing', { img_url: '/workspace/old.png', relative_path: 'old.png' });
    const terminalWithoutReceipt = wire('Success');
    const merged = composeMessage(terminalWithoutReceipt, [progress]);
    const message = merged[0];
    if (message.type !== 'tool_group') throw new Error('expected tool group');
    expect(message.content[0].status).toBe('Success');
    expect(message.content[0].result_display).toBeUndefined();
  });

  test('serializes structured text payloads instead of leaking objects into message content', () => {
    const message = transformMessage(
      baseWire({
        type: 'text',
        data: { command: 'codex --version' },
      })
    );

    expect(message?.type).toBe('text');
    if (message?.type !== 'text') throw new Error('expected text message');
    expect(message.content.content).toBe('{\n  "command": "codex --version"\n}');
  });

  test('serializes non-string rich text content while preserving string metadata only', () => {
    const message = transformMessage(
      baseWire({
        type: 'content',
        data: {
          content: { text: 'hello' },
          sender_name: { bad: true },
          sender_backend: 'codex',
          sender_conversation_id: 'not-a-number',
        },
      })
    );

    expect(message?.type).toBe('text');
    if (message?.type !== 'text') throw new Error('expected text message');
    expect(message.content.content).toBe('{\n  "text": "hello"\n}');
    expect(message.content.senderName).toBeUndefined();
    expect(message.content.senderAgentType).toBe('codex');
    expect(message.content.senderConversationId).toBeUndefined();
  });

  test('maps external collaboration fields to the single Agent message shape', () => {
    const message = transformMessage(
      baseWire({
        type: 'content',
        data: {
          content: 'Delegated result',
          teammate_message: true,
          sender_name: 'Researcher',
          sender_backend: 'nomi',
          sender_conversation_id: '0190f5fe-7c00-7a00-8000-000000000007',
        },
      })
    );

    expect(message?.type).toBe('text');
    if (message?.type !== 'text') throw new Error('expected text message');
    expect(message.content).toMatchObject({
      content: 'Delegated result',
      agentMessage: true,
      senderName: 'Researcher',
      senderAgentType: 'nomi',
      senderConversationId: '0190f5fe-7c00-7a00-8000-000000000007',
    });
  });

  test('normalizes tips content and type from malformed payloads', () => {
    const message = transformMessage(
      baseWire({
        type: 'tips',
        data: {
          content: { message: 'rate limited' },
          type: 'unexpected',
        },
      })
    );

    expect(message?.type).toBe('tips');
    if (message?.type !== 'tips') throw new Error('expected tips message');
    expect(message.content.type).toBe('warning');
    expect(message.content.content).toBe('{\n  "message": "rate limited"\n}');
  });

  test('preserves canonical message and owning turn identities for terminal errors', () => {
    const terminalMessageId = parseMessageId('019b0000-0000-7000-8000-000000000010');
    const turnId = parseMessageId('019b0000-0000-7000-8000-000000000011');
    const message = transformMessage(
      baseWire({
        msg_id: terminalMessageId,
        turn_id: turnId,
        type: 'error',
        data: { message: 'rate limited', code: 'USER_LLM_PROVIDER_RATE_LIMITED' },
      })
    );

    expect(message?.type).toBe('tips');
    expect(message?.id).not.toBe(terminalMessageId);
    expect(message?.msg_id).toBe(terminalMessageId);
    expect(message?.turn_id).toBe(turnId);
  });

  test('preserves owning turn identity on non-terminal stream rows', () => {
    const turnId = parseMessageId('019b0000-0000-7000-8000-000000000012');
    const message = transformMessage(
      baseWire({
        turn_id: turnId,
        type: 'tool_call',
        data: { call_id: 'tool-1', name: 'Generate', status: 'running' },
      })
    );

    expect(message?.type).toBe('tool_call');
    expect(message?.turn_id).toBe(turnId);
  });

  test('normalizes thinking content, subject, status, and duration defensively', () => {
    const message = transformMessage(
      baseWire({
        type: 'thinking',
        data: {
          content: { step: 'scan' },
          subject: { title: 'Audit' },
          status: 'bad-status',
          duration_ms: '500',
        },
      })
    );

    expect(message?.type).toBe('thinking');
    if (message?.type !== 'thinking') throw new Error('expected thinking message');
    expect(message.content.content).toBe('{\n  "step": "scan"\n}');
    expect(message.content.subject).toBe('{\n  "title": "Audit"\n}');
    expect(message.content.status).toBe('thinking');
    expect(message.content.duration).toBeUndefined();
  });

  test('drops malformed tool_group content to an empty array', () => {
    const message = transformMessage(
      baseWire({
        type: 'tool_group',
        data: { call_id: 'tool-1', status: 'Executing' },
      })
    );

    expect(message?.type).toBe('tool_group');
    if (message?.type !== 'tool_group') throw new Error('expected tool_group message');
    expect(message.content).toEqual([]);
  });

  test('preserves disconnected agent status so historical rows stay hidden', () => {
    const message = transformMessage(
      baseWire({
        type: 'agent_status',
        data: {
          backend: { name: 'codex' },
          status: 'disconnected',
        },
      })
    );

    expect(message?.type).toBe('agent_status');
    if (message?.type !== 'agent_status') throw new Error('expected agent_status message');
    expect(message.content.backend).toBe('{\n  "name": "codex"\n}');
    expect(message.content.status).toBe('disconnected');
  });

  test('converts knowledge writeback events into assistant message status updates', () => {
    const message = transformKnowledgeWritebackEvent({
      conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000001'),
      msg_id: MESSAGE_ID,
      status: 'writing',
      attempt_id: 'attempt-1',
      started_at: 1000,
      updated_at: 1200,
      retryable: false,
      candidates: 2,
    });

    expect(message?.type).toBe('text');
    expect(message?.msg_id).toBe(MESSAGE_ID);
    expect(message?.content.content).toBe('');
    expect(message?.content.knowledge_writeback?.status).toBe('writing');
    expect(message?.content.knowledge_writeback?.attempt_id).toBe('attempt-1');
  });

  test('preserves persisted knowledge writeback state when hydrating text messages', () => {
    const message = transformMessage(
      baseWire({
        type: 'content',
        data: {
          content: 'Final answer.',
          knowledge_writeback: {
            status: 'failed',
            attempt_id: 'attempt-1',
            retryable: true,
            failures: [{
              kb_id: parseKnowledgeBaseId('019b0000-0000-7000-8000-000000000001'),
              rel_path: 'notes.md',
              error: 'disk full',
            }],
          },
        },
      })
    );

    expect(message?.type).toBe('text');
    if (message?.type !== 'text') throw new Error('expected text message');
    expect(message.content.content).toBe('Final answer.');
    expect(message.content.knowledge_writeback?.status).toBe('failed');
    expect(message.content.knowledge_writeback?.retryable).toBe(true);
    expect(message.content.knowledge_writeback?.failures?.[0]?.error).toBe('disk full');
  });

  test('converts live user-created events into right-side messages for the active conversation', () => {
    const message = transformUserCreatedEvent(
      {
        conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000002'),
        msg_id: MESSAGE_ID,
        content: 'from IM',
        position: 'right',
        status: 'finish',
        channel_platform: 'telegram',
        companion: true,
        companion_id: COMPANION_ID,
        created_at: 1234,
      },
      parseConversationId('0190f5fe-7c00-7a00-8000-000000000002')
    );

    expect(message?.type).toBe('text');
    if (message?.type !== 'text') throw new Error('expected text message');
    expect(message.conversation_id).toBe('0190f5fe-7c00-7a00-8000-000000000002');
    expect(message.msg_id).toBe(MESSAGE_ID);
    expect(message.position).toBe('right');
    expect(message.status).toBe('finish');
    expect(message.created_at).toBe(1234);
    expect(message.content.content).toBe('from IM');
  });

  test('ignores user-created events for other conversations and hidden messages', () => {
    const baseEvent = {
      conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000002'),
      msg_id: MESSAGE_ID,
      content: 'from IM',
      position: 'right' as const,
      status: 'finish',
      created_at: 1234,
    };

    expect(transformUserCreatedEvent(baseEvent, parseConversationId('0190f5fe-7c00-7a00-8000-000000000003'))).toBeUndefined();
    expect(transformUserCreatedEvent({ ...baseEvent, hidden: true }, parseConversationId('0190f5fe-7c00-7a00-8000-000000000002'))).toBeUndefined();
  });
});
