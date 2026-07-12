/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { composeMessage, transformKnowledgeWritebackEvent, transformMessage, transformUserCreatedEvent } from './chatLib';

const baseWire = (overrides: Record<string, unknown>) =>
  ({
    msg_id: 'msg-1',
    conversation_id: 1,
    ...overrides,
  }) as any;

describe('transformMessage runtime field normalization', () => {
  test('composeMessage keeps reused tool call ids isolated by turn', () => {
    const first = transformMessage(baseWire({
      msg_id: 'turn-1',
      type: 'tool_call',
      data: { call_id: 'call-1', name: 'Read', status: 'completed' },
    }))!;
    const second = transformMessage(baseWire({
      msg_id: 'turn-2',
      type: 'tool_call',
      data: { call_id: 'call-1', name: 'Read', status: 'running' },
    }))!;

    expect(composeMessage(second, [first])).toHaveLength(2);
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
      conversation_id: 1,
      msg_id: 'assistant-turn-1',
      status: 'writing',
      attempt_id: 'attempt-1',
      started_at: 1000,
      updated_at: 1200,
      retryable: false,
      candidates: 2,
    });

    expect(message?.type).toBe('text');
    expect(message?.msg_id).toBe('assistant-turn-1');
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
            failures: [{ kb_id: 'kb_1', rel_path: 'notes.md', error: 'disk full' }],
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
        conversation_id: 7,
        msg_id: 'msg-im-1',
        content: 'from IM',
        position: 'right',
        status: 'finish',
        channel_platform: 'telegram',
        companion: true,
        companion_id: 'companion-1',
        created_at: 1234,
      },
      7
    );

    expect(message?.type).toBe('text');
    if (message?.type !== 'text') throw new Error('expected text message');
    expect(message.conversation_id).toBe(7);
    expect(message.msg_id).toBe('msg-im-1');
    expect(message.position).toBe('right');
    expect(message.status).toBe('finish');
    expect(message.created_at).toBe(1234);
    expect(message.content.content).toBe('from IM');
  });

  test('ignores user-created events for other conversations and hidden messages', () => {
    const baseEvent = {
      conversation_id: 7,
      msg_id: 'msg-im-1',
      content: 'from IM',
      position: 'right' as const,
      status: 'finish',
      created_at: 1234,
    };

    expect(transformUserCreatedEvent(baseEvent, 8)).toBeUndefined();
    expect(transformUserCreatedEvent({ ...baseEvent, hidden: true }, 7)).toBeUndefined();
  });
});
