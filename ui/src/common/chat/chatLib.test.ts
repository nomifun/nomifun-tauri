/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { transformMessage } from './chatLib';

const baseWire = (overrides: Record<string, unknown>) =>
  ({
    msg_id: 'msg-1',
    conversation_id: 1,
    ...overrides,
  }) as any;

describe('transformMessage runtime field normalization', () => {
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
});
