/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMessageAcpToolCall, IMessageToolCall, IMessageToolGroup, TMessage } from '@/common/chat/chatLib';
import { normalizeToolMessages } from '@/common/chat/normalizeToolCall';
import type { TurnDisclosureProcessState } from './turnDisclosureModel';

type ToolProcessMessage = IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall;

type ProcessStateItem =
  | TMessage
  | { type: 'file_summary' }
  | { type: 'tool_summary'; messages: ToolProcessMessage[] }
  | { type: 'artifact' };

export const mergeProcessStates = (states: TurnDisclosureProcessState[]): TurnDisclosureProcessState => {
  if (states.includes('waiting')) return 'waiting';
  if (states.includes('running')) return 'running';
  if (states.includes('failed')) return 'failed';
  if (states.includes('canceled')) return 'canceled';
  return 'completed';
};

export const getToolMessagesProcessState = (messages: ToolProcessMessage[]): TurnDisclosureProcessState => {
  const rawStates = messages.flatMap((message): TurnDisclosureProcessState[] => {
    if (message.type !== 'tool_group') return [];
    if (!Array.isArray(message.content)) return [];
    return message.content.map((tool) => {
      if (tool.status === 'Confirming') return 'waiting';
      if (tool.status === 'Executing' || tool.status === 'Pending') return 'running';
      if (tool.status === 'Error') return tool.confirmationDetails?.type === 'exec' ? 'completed' : 'failed';
      if (tool.status === 'Canceled') return 'canceled';
      return 'completed';
    });
  });

  const normalizedStates = normalizeToolMessages(messages).map((tool): TurnDisclosureProcessState => {
    if (tool.status === 'running' || tool.status === 'pending') return 'running';
    if (tool.nonFatalFailure) return 'completed';
    if (tool.status === 'error') return 'failed';
    if (tool.status === 'canceled') return 'canceled';
    return 'completed';
  });

  return mergeProcessStates([...rawStates, ...normalizedStates]);
};

export const getProcessItemState = (item: ProcessStateItem): TurnDisclosureProcessState => {
  if ('type' in item && item.type === 'tool_summary') {
    return getToolMessagesProcessState(item.messages);
  }
  if ('type' in item && item.type === 'file_summary') {
    return 'completed';
  }
  if ('type' in item && item.type === 'artifact') {
    return 'completed';
  }

  switch (item.type) {
    case 'thinking':
      return item.content.status === 'done' ? 'completed' : 'running';
    case 'tool_call':
      return getToolMessagesProcessState([item]);
    case 'tool_group':
      return getToolMessagesProcessState([item]);
    case 'acp_tool_call':
      return getToolMessagesProcessState([item]);
    case 'permission':
    case 'acp_permission':
      return 'waiting';
    case 'agent_status':
      if (item.content.status === 'error') return 'failed';
      if (item.content.status === 'connecting' || item.content.status === 'preparing') return 'running';
      return 'completed';
    case 'tips':
      return item.content.type === 'error' ? 'failed' : 'completed';
    default:
      return 'completed';
  }
};
