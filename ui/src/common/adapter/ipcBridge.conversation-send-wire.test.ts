/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { parseConversationId, parseMessageId } from '@/common/types/ids';
import { conversation } from './ipcBridge';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');
const sendStart = source.indexOf('  sendMessage: {');
const sendEnd = source.indexOf('  steer:', sendStart);
const sendMessageSource = source.slice(sendStart, sendEnd);
const realFetch = globalThis.fetch;

describe('conversation send-message wire contract', () => {
  test('keeps idempotency metadata out of the strict JSON DTO', () => {
    expect(sendStart).toBeGreaterThan(-1);
    expect(sendEnd).toBeGreaterThan(sendStart);
    expect(sendMessageSource.includes('content: p.input')).toBe(true);
    expect(sendMessageSource.includes('files: p.files')).toBe(true);
    expect(sendMessageSource.includes('inject_skills: p.inject_skills')).toBe(true);
    expect(sendMessageSource.includes('initial_only:')).toBe(false);
    expect(sendMessageSource.includes('loading_id:')).toBe(false);
    expect(sendMessageSource.includes('idempotency_key:')).toBe(false);
  });

  test('requires idempotency_key and never falls back to a body or loading id', () => {
    expect(source.includes('idempotency_key: string;')).toBe(true);
    expect(source.includes('loading_id?: string;')).toBe(false);
    expect(
      sendMessageSource.includes(
        'const idempotencyKey = requireConversationIdempotencyKey(p.idempotency_key);'
      )
    ).toBe(true);
    expect(sendMessageSource.includes('p.loading_id')).toBe(false);
    expect(
      sendMessageSource.includes(
        '{ idempotencyKey, initialOnly: p.initial_only === true }'
      )
    ).toBe(true);
  });

  test('preserves durable replay and terminal result metadata', async () => {
    const msgId = '0190f5fe-7c00-7a00-8000-000000000201';
    try {
      globalThis.fetch = (() =>
        Promise.resolve(
          new Response(
            JSON.stringify({
              success: true,
              data: {
                msg_id: msgId,
                replayed: true,
                completed: true,
                result_ok: false,
                result_text: 'terminal result',
                result_error: 'provider failed',
              },
            }),
            { status: 202, headers: { 'Content-Type': 'application/json' } }
          )
        )) as unknown as typeof fetch;

      const result = await conversation.sendMessage.invoke({
        input: 'retry',
        conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000202'),
        idempotency_key: '0190f5fe-7c00-7a00-8000-000000000203',
      });

      expect(result).toEqual({
        msg_id: msgId,
        replayed: true,
        completed: true,
        result_ok: false,
        result_text: 'terminal result',
        result_error: 'provider failed',
      });
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('preserves the same replay contract for steer and edit-resubmit', async () => {
    const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000207');
    const targetMessageId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000208');
    const canonicalMessageId = '0190f5fe-7c00-7a00-8000-000000000209';
    const expected = {
      msg_id: canonicalMessageId,
      replayed: true,
      completed: true,
      result_ok: true,
      result_text: 'already delivered',
      result_error: null,
    };
    try {
      globalThis.fetch = (() =>
        Promise.resolve(
          new Response(
            JSON.stringify({
              success: true,
              data: expected,
            }),
            { status: 202, headers: { 'Content-Type': 'application/json' } }
          )
        )) as unknown as typeof fetch;

      const steerResult = await conversation.steer.invoke({
        input: 'retry steer',
        conversation_id: conversationId,
        idempotency_key: '0190f5fe-7c00-7a00-8000-000000000210',
      });
      const editResult = await conversation.editResubmit.invoke({
        input: 'retry edit',
        conversation_id: conversationId,
        msg_id: targetMessageId,
        idempotency_key: '0190f5fe-7c00-7a00-8000-000000000211',
      });

      expect(steerResult).toEqual(expected);
      expect(editResult).toEqual(expected);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test('fails closed when a legacy response omits replay authority', async () => {
    const msgId = '0190f5fe-7c00-7a00-8000-000000000204';
    try {
      globalThis.fetch = (() =>
        Promise.resolve(
          new Response(
            JSON.stringify({
              success: true,
              data: { msg_id: msgId },
            }),
            { status: 202, headers: { 'Content-Type': 'application/json' } }
          )
        )) as unknown as typeof fetch;

      const result = await conversation.sendMessage.invoke({
        input: 'legacy retry',
        conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000205'),
        idempotency_key: '0190f5fe-7c00-7a00-8000-000000000206',
      });

      expect(result).toEqual({
        msg_id: msgId,
        replayed: true,
        completed: false,
        result_ok: null,
        result_text: null,
        result_error: null,
      });
    } finally {
      globalThis.fetch = realFetch;
    }
  });
});
