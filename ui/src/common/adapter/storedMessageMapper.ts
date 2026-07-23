/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TMessage } from '@/common/chat/chatLib';
import {
  parseConversationId,
  parseMessageId,
  type MessageId,
} from '@/common/types/ids';
import { uuid } from '@/common/utils';

export type StoredMessageResponse = {
  message_id: unknown;
  conversation_id: unknown;
  msg_id?: unknown;
  type: unknown;
  content: unknown;
  position?: unknown;
  status?: unknown;
  hidden: unknown;
  created_at: unknown;
};

const STORED_MESSAGE_TYPES = new Set<TMessage['type']>([
  'text',
  'tips',
  'tool_call',
  'tool_group',
  'agent_status',
  'permission',
  'acp_permission',
  'acp_tool_call',
  'plan',
  'thinking',
  'available_commands',
]);

const STORED_MESSAGE_POSITIONS = new Set<NonNullable<TMessage['position']>>([
  'left',
  'right',
  'center',
  'pop',
]);

const STORED_MESSAGE_STATUSES = new Set<NonNullable<TMessage['status']>>([
  'finish',
  'pending',
  'error',
  'work',
]);

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const parseStoredMessageType = (value: unknown): TMessage['type'] => {
  if (typeof value !== 'string' || !STORED_MESSAGE_TYPES.has(value as TMessage['type'])) {
    throw new TypeError(`Invalid persisted message type: ${String(value)}`);
  }
  return value as TMessage['type'];
};

const parseStoredMessagePosition = (
  value: unknown
): TMessage['position'] => {
  if (value == null) return undefined;
  if (
    typeof value !== 'string' ||
    !STORED_MESSAGE_POSITIONS.has(value as NonNullable<TMessage['position']>)
  ) {
    throw new TypeError(`Invalid persisted message position: ${String(value)}`);
  }
  return value as NonNullable<TMessage['position']>;
};

const parseStoredMessageStatus = (value: unknown): TMessage['status'] => {
  if (value == null) return undefined;
  if (
    typeof value !== 'string' ||
    !STORED_MESSAGE_STATUSES.has(value as NonNullable<TMessage['status']>)
  ) {
    throw new TypeError(`Invalid persisted message status: ${String(value)}`);
  }
  return value as NonNullable<TMessage['status']>;
};

const parseStoredMessageContent = (
  type: TMessage['type'],
  value: unknown
): { content: TMessage['content']; turnId?: MessageId } => {
  if (type === 'tool_group') {
    if (!Array.isArray(value)) {
      throw new TypeError('Invalid persisted tool_group content');
    }
    return { content: value as TMessage['content'] };
  }
  if (!isRecord(value)) {
    throw new TypeError('Invalid persisted message content');
  }

  const { turn_id: rawTurnId, ...content } = value;
  return {
    content: content as TMessage['content'],
    ...(rawTurnId == null ? {} : { turnId: parseMessageId(rawTurnId) }),
  };
};

/**
 * Creates a v3 stored-message boundary mapper.
 *
 * Durable message UUIDv7s and renderer-local keys remain separate. The closure
 * keeps a stable local key for repeated fetches of the same durable row during
 * the current renderer lifetime without turning the business ID into a React
 * key or inventing a prefixed entity ID.
 */
export const createStoredMessageMapper = (
  createRenderKey: () => string = () => uuid(16)
) => {
  const renderKeyByMessageId = new Map<MessageId, string>();

  return (message: StoredMessageResponse): TMessage => {
    const type = parseStoredMessageType(message.type);
    if (typeof message.hidden !== 'boolean') {
      throw new TypeError('Invalid persisted message hidden flag');
    }
    if (typeof message.created_at !== 'number' || !Number.isFinite(message.created_at)) {
      throw new TypeError('Invalid persisted message created_at');
    }

    const messageId = parseMessageId(message.message_id);
    let renderKey = renderKeyByMessageId.get(messageId);
    if (renderKey == null) {
      renderKey = createRenderKey();
      if (!renderKey) {
        throw new TypeError('Persisted message render key must be non-empty');
      }
      renderKeyByMessageId.set(messageId, renderKey);
    }

    const { content, turnId } = parseStoredMessageContent(type, message.content);
    const position = parseStoredMessagePosition(message.position);
    const status = parseStoredMessageStatus(message.status);

    return {
      id: renderKey,
      message_id: messageId,
      conversation_id: parseConversationId(message.conversation_id),
      msg_id: message.msg_id == null ? undefined : parseMessageId(message.msg_id),
      type,
      content,
      ...(position == null ? {} : { position }),
      ...(status == null ? {} : { status }),
      hidden: message.hidden,
      created_at: message.created_at,
      ...(turnId == null ? {} : { turn_id: turnId }),
    } as TMessage;
  };
};

export const fromApiStoredMessage = createStoredMessageMapper();
