/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { transformMessage } from '@/common/chat/chatLib';
import { parseConversationId, parseMessageId } from '@/common/types/ids';
import { getNomiToolGroupRuntimeState } from './useNomiMessage';

const CONVERSATION_ID = parseConversationId('0190f5fe-7c00-7a00-8000-000000000001');
const MESSAGE_ID = parseMessageId('0190f5fe-7c00-7a00-8000-000000000002');

const transformWireToolGroup = (status: 'running' | 'completed' | 'error') => {
  const message = transformMessage({
    conversation_id: CONVERSATION_ID,
    msg_id: MESSAGE_ID,
    type: 'tool_group',
    data: [{ call_id: 'call-1', name: 'Read', description: 'src/main.ts', status }],
  } as any);

  expect(message?.type).toBe('tool_group');
  if (message?.type !== 'tool_group') throw new Error('expected tool_group message');
  return message.content[0];
};

describe('Nomi tool_group wire contract', () => {
  test('maps the backend snake_case status vocabulary into the UI vocabulary', () => {
    expect(transformWireToolGroup('running').status).toBe('Executing');
    expect(transformWireToolGroup('completed').status).toBe('Success');
    expect(transformWireToolGroup('error').status).toBe('Error');
  });

  test('derives live activity from backend wire statuses rather than legacy display labels', () => {
    expect(getNomiToolGroupRuntimeState([{ call_id: 'call-1', status: 'running' }]).hasActive).toBe(true);
    expect(getNomiToolGroupRuntimeState([{ call_id: 'call-1', status: 'completed' }]).hasActive).toBe(false);
    expect(getNomiToolGroupRuntimeState([{ call_id: 'call-1', status: 'error' }]).hasActive).toBe(false);
  });
});
