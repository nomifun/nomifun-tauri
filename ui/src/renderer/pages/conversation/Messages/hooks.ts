/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { AgentStreamErrorInfo, IMessageThinking, IMessageTips, TMessage } from '@/common/chat/chatLib';
import { toDisplayText } from '@/common/chat/displayText';
import {
  composeMessage,
  mergeAcpToolCallContent,
  mergeTextMessageContent,
  normalizeKnowledgeWritebackState,
  normalizeAgentStreamError,
  preferTextMessageVersion,
  transformKnowledgeWritebackEvent,
} from '@/common/chat/chatLib';
import { useCallback, useEffect, useRef, useState } from 'react';
import { createContext } from '@renderer/utils/ui/createContext';

const [useMessageList, MessageListProvider, useUpdateMessageList] = createContext([] as TMessage[]);
const [useMessageListLoading, MessageListLoadingProvider, useUpdateMessageListLoading] = createContext(false);

const beforeUpdateMessageListStack: Array<(list: TMessage[]) => TMessage[]> = [];

// 消息索引缓存类型定义
// Message index cache type definitions
interface MessageIndex {
  msgIdIndex: Map<string, number>; // msg_id -> index
  call_idIndex: Map<string, number>; // tool_call.call_id -> index
  tool_call_idIndex: Map<string, number>; // acp_tool_call.update.tool_call_id -> index
  permission_call_idIndex: Map<string, number>; // permission.content.call_id -> index
}

function getMessageIndexKey(message: TMessage): string | undefined {
  if (!message.msg_id) return undefined;
  if (message.type === 'thinking') return `thinking:${message.msg_id}`;
  if (message.type === 'agent_status') {
    const backend = typeof message.content?.backend === 'string' ? message.content.backend : 'agent';
    return `agent_status:${message.msg_id}:${backend}`;
  }
  return message.msg_id;
}

const compactThinkingStreamText = (value: unknown): string => toDisplayText(value).replace(/\s+/g, ' ').trim();

export function mergeThinkingStreamContent(existing: unknown, incoming: unknown): string {
  const existingText = toDisplayText(existing);
  const incomingText = toDisplayText(incoming);
  if (!incomingText) return existingText;
  if (!existingText) return incomingText;
  if (incomingText === existingText) return existingText;
  const existingCompact = compactThinkingStreamText(existingText);
  const incomingCompact = compactThinkingStreamText(incomingText);
  if (incomingCompact === existingCompact) return existingText;
  if (incomingCompact && existingCompact.startsWith(incomingCompact)) return existingText;
  if (existingCompact && incomingCompact.startsWith(existingCompact)) return incomingText;
  if (incomingText.startsWith(existingText)) return incomingText;
  return existingText + incomingText;
}

// 使用 WeakMap 缓存索引，当列表被 GC 时自动清理
// Use WeakMap to cache index, auto-cleanup when list is GC'd
const indexCache = new WeakMap<TMessage[], MessageIndex>();

export function logDroppedToolCallWithoutCallId(message: TMessage | undefined): boolean {
  if (!message) return false;
  if (message.type !== 'tool_call' || message.content?.call_id) return false;

  console.warn('[tool-call] dropped tool_call without call_id', {
    conversation_id: message.conversation_id,
    msg_id: message.msg_id,
    name: message.content?.name,
    status: message.content?.status,
  });
  return true;
}

// 构建消息索引
// Build message index
function buildMessageIndex(list: TMessage[]): MessageIndex {
  const msgIdIndex = new Map<string, number>();
  const call_idIndex = new Map<string, number>();
  const tool_call_idIndex = new Map<string, number>();
  const permission_call_idIndex = new Map<string, number>();

  for (let i = 0; i < list.length; i++) {
    const msg = list[i];
    const msgIndexKey = getMessageIndexKey(msg);
    if (msgIndexKey) {
      msgIdIndex.set(msgIndexKey, i);
    }
    if (msg.type === 'tool_call' && msg.content?.call_id) {
      call_idIndex.set(msg.content.call_id, i);
    }
    if (msg.type === 'acp_tool_call' && msg.content?.update?.tool_call_id) {
      tool_call_idIndex.set(msg.content.update.tool_call_id, i);
    }
    if (msg.type === 'permission' && msg.content?.call_id) {
      permission_call_idIndex.set(msg.content.call_id, i);
    }
  }

  return { msgIdIndex, call_idIndex, tool_call_idIndex, permission_call_idIndex };
}

// 获取或构建索引（带缓存）
// Get or build index with caching
function getOrBuildIndex(list: TMessage[]): MessageIndex {
  let cached = indexCache.get(list);
  if (!cached) {
    cached = buildMessageIndex(list);
    indexCache.set(list, cached);
  }
  return cached;
}

// 使用索引优化的消息合并函数
// Index-optimized message compose function
function composeMessageWithIndex(message: TMessage | undefined, list: TMessage[], index: MessageIndex): TMessage[] {
  if (!message) return list || [];

  if (logDroppedToolCallWithoutCallId(message)) {
    return list || [];
  }

  if (message.type === 'text' && message.content.knowledge_writeback && message.msg_id) {
    const existingIdx = index.msgIdIndex.get(message.msg_id);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'text') {
        const newList = list.slice();
        newList[existingIdx] = {
          ...existingMsg,
          content: {
            ...mergeTextMessageContent(existingMsg.content, message.content),
            content: existingMsg.content.content,
          },
        };
        return newList;
      }
    }

    const newIdx = list.length;
    index.msgIdIndex.set(message.msg_id, newIdx);
    return list.concat(message);
  }

  if (!list?.length) {
    // Update index when adding first message
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) {
      index.msgIdIndex.set(msgIndexKey, 0);
    }
    return [message];
  }

  const last = list[list.length - 1];

  // 对于 tool_group 类型，使用原始的 composeMessage（因为涉及内部数组匹配）
  // For tool_group type, use original composeMessage (involves inner array matching)
  // After composeMessage, the returned list may have different length/ordering,
  // so we must invalidate the index to prevent stale lookups in subsequent calls.
  if (message.type === 'tool_group') {
    const result = composeMessage(message, list);
    if (result !== list) {
      // Rebuild index maps from the new list to keep them in sync
      const rebuilt = buildMessageIndex(result);
      index.msgIdIndex = rebuilt.msgIdIndex;
      index.call_idIndex = rebuilt.call_idIndex;
      index.tool_call_idIndex = rebuilt.tool_call_idIndex;
      index.permission_call_idIndex = rebuilt.permission_call_idIndex;
    }
    return result;
  }

  // tool_call: 使用 call_idIndex 快速查找
  // tool_call: use call_idIndex for fast lookup
  if (message.type === 'tool_call' && message.content?.call_id) {
    const existingIdx = index.call_idIndex.get(message.content.call_id);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'tool_call') {
        const newList = list.slice();
        const merged = { ...existingMsg.content, ...message.content };
        newList[existingIdx] = { ...existingMsg, content: merged };
        return newList;
      }
    }
    // 未找到，添加新消息并更新索引
    const newIdx = list.length;
    index.call_idIndex.set(message.content.call_id, newIdx);
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
    return list.concat(message);
  }

  // acp_tool_call: use tool_call_idIndex for fast lookup
  if (message.type === 'acp_tool_call' && message.content?.update?.tool_call_id) {
    const existingIdx = index.tool_call_idIndex.get(message.content.update.tool_call_id);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'acp_tool_call') {
        const newList = list.slice();
        const merged = mergeAcpToolCallContent(existingMsg.content, message.content);
        newList[existingIdx] = { ...existingMsg, content: merged };
        return newList;
      }
    }
    // 未找到，添加新消息并更新索引
    const newIdx = list.length;
    index.tool_call_idIndex.set(message.content.update.tool_call_id, newIdx);
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
    return list.concat(message);
  }

  // permission: use call_id for recovery/live stream dedupe.
  if (message.type === 'permission' && message.content?.call_id) {
    const existingIdx = index.permission_call_idIndex.get(message.content.call_id);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'permission') {
        const newList = list.slice();
        newList[existingIdx] = { ...existingMsg, ...message, content: message.content };
        return newList;
      }
    }
    const newIdx = list.length;
    index.permission_call_idIndex.set(message.content.call_id, newIdx);
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
    return list.concat(message);
  }

  // text message: merge only with the latest contiguous streaming chunk.
  // text 消息: 只与最后一条连续的流式片段合并，保留被工具/思考打断后的消息边界。
  if (message.type === 'text' && message.msg_id) {
    const existingIdx = index.msgIdIndex.get(message.msg_id);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'text') {
        const existingIsWritebackOnly =
          existingMsg.position === 'left' &&
          existingMsg.content.content.length === 0 &&
          Boolean(existingMsg.content.knowledge_writeback);
        if (existingIsWritebackOnly && message.position === 'left') {
          const newList = list.slice();
          newList[existingIdx] = {
            ...existingMsg,
            ...message,
            id: existingMsg.id,
            content: mergeTextMessageContent(existingMsg.content, message.content),
          };
          return newList;
        }
        // User messages (right position) are complete — skip if already exists to prevent duplicates
        if (message.position === 'right') {
          return list;
        }
        // Complete teammate messages are not streaming chunks — skip if already exists
        if ((message.content as { teammateMessage?: boolean })?.teammateMessage) {
          return list;
        }
      }
    }

    if (last.type === 'text' && last.msg_id === message.msg_id) {
      const newList = list.slice();
      newList[newList.length - 1] = {
        ...last,
        content: mergeTextMessageContent(last.content, message.content),
      };
      return newList;
    }

    const newIdx = list.length;
    index.msgIdIndex.set(message.msg_id, newIdx);
    return list.concat(message);
  }

  // thinking message: merge only with the latest contiguous thinking chunk.
  // Uses "thinking:${msg_id}" key to avoid collision with text messages sharing the same msg_id.
  if (message.type === 'thinking' && message.msg_id) {
    const thinkingKey = `thinking:${message.msg_id}`;
    if (message.content.status === 'done') {
      const existingIdx = index.msgIdIndex.get(thinkingKey);
      if (existingIdx !== undefined && existingIdx < list.length) {
        const existingMsg = list[existingIdx];
        if (existingMsg.type === 'thinking') {
          const newList = list.slice();
          newList[existingIdx] = {
            ...existingMsg,
            content: {
              ...existingMsg.content,
              status: 'done' as const,
              duration: message.content.duration,
              subject: message.content.subject || existingMsg.content.subject,
            },
          };
          return newList;
        }
      }
    }

    if (last.type === 'thinking' && last.msg_id === message.msg_id) {
      const nextContent = mergeThinkingStreamContent(last.content.content, message.content.content);
      const newList = list.slice();
      newList[newList.length - 1] = {
        ...last,
        content: {
          ...last.content,
          content: nextContent,
          subject: message.content.subject || last.content.subject,
        },
      };
      return newList;
    }

    const newIdx = list.length;
    index.msgIdIndex.set(thinkingKey, newIdx);
    return list.concat(message);
  }

  // plan message: update content and move to end of list
  if (message.type === 'plan' && message.msg_id) {
    const existingIdx = index.msgIdIndex.get(message.msg_id);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      const newList = list.slice();
      newList.splice(existingIdx, 1);
      const updated = { ...existingMsg, ...message, content: message.content } as TMessage;
      newList.push(updated);
      // Rebuild index after splice
      const rebuilt = buildMessageIndex(newList);
      index.msgIdIndex = rebuilt.msgIdIndex;
      index.call_idIndex = rebuilt.call_idIndex;
      index.tool_call_idIndex = rebuilt.tool_call_idIndex;
      index.permission_call_idIndex = rebuilt.permission_call_idIndex;
      return newList;
    }
    const newIdx = list.length;
    index.msgIdIndex.set(message.msg_id, newIdx);
    return list.concat(message);
  }

  // agent_status / tips and other msg-id-keyed messages:
  // replace only the same keyed process item, never a text/tool item that
  // happens to belong to the same turn msg_id.
  const msgIndexKey = getMessageIndexKey(message);
  if (msgIndexKey) {
    const existingIdx = index.msgIdIndex.get(msgIndexKey);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      const newList = list.slice();
      newList[existingIdx] = {
        ...existingMsg,
        ...message,
        content: message.content,
      } as TMessage;
      return newList;
    }
  }

  // Other types: fallback to last message check
  // 其他类型: 回退到检查最后一条消息
  if (last.msg_id !== message.msg_id || last.type !== message.type) {
    // Add new message and update index
    const newIdx = list.length;
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
    return list.concat(message);
  }

  // Merge other message types with same msg_id
  const newList = list.slice();
  const lastIdx = newList.length - 1;
  newList[lastIdx] = { ...last, ...message };
  return newList;
}

export function composeMessageForTest(message: TMessage | undefined, list: TMessage[]): TMessage[] {
  return composeMessageWithIndex(message, list, buildMessageIndex(list));
}

export const useAddOrUpdateMessage = () => {
  const update = useUpdateMessageList();
  const pendingRef = useRef<Array<{ message: TMessage; add: boolean }>>([]);
  const rafRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(() => {
    rafRef.current = null;

    const pending = pendingRef.current;
    if (!pending.length) return;
    pendingRef.current = [];
    update((list) => {
      // 获取或构建索引用于快速查找 (O(1) instead of O(n))
      // Get or build index for fast lookup
      const index = getOrBuildIndex(list);
      let newList = list;

      for (const item of pending) {
        if (!item.message) {
          continue;
        }

        if (logDroppedToolCallWithoutCallId(item.message)) {
          continue;
        }

        if (item.add) {
          // 新增消息，更新索引
          // New message, update index
          const msg = item.message;
          const newIdx = newList.length;
          const msgIndexKey = getMessageIndexKey(msg);
          if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
          if (msg.type === 'tool_call' && msg.content?.call_id) {
            index.call_idIndex.set(msg.content.call_id, newIdx);
          }
          if (msg.type === 'acp_tool_call' && msg.content?.update?.tool_call_id) {
            index.tool_call_idIndex.set(msg.content.update.tool_call_id, newIdx);
          }
          if (msg.type === 'permission' && msg.content?.call_id) {
            index.permission_call_idIndex.set(msg.content.call_id, newIdx);
          }
          newList = newList.concat(msg);
        } else {
          // 使用索引优化的消息合并
          // Use index-optimized message compose
          newList = composeMessageWithIndex(item.message, newList, index);
        }

        while (beforeUpdateMessageListStack.length) {
          newList = beforeUpdateMessageListStack.shift()!(newList);
        }
      }
      return newList;
    });

    rafRef.current = setTimeout(flush);
  }, []);

  useEffect(() => {
    return () => {
      if (rafRef.current !== null) {
        clearTimeout(rafRef.current);
      }
    };
  }, []);

  return useCallback(
    (message: TMessage | undefined, add = false) => {
      if (!message) {
        return;
      }
      pendingRef.current.push({ message, add });
      if (rafRef.current === null) {
        rafRef.current = setTimeout(flush);
      }
    },
    [flush]
  );
};

export const useKnowledgeWritebackEvents = (conversationId: number | undefined) => {
  const addOrUpdateMessage = useAddOrUpdateMessage();

  useEffect(() => {
    if (!conversationId) return;
    return ipcBridge.conversation.knowledgeWriteback.on((event) => {
      if (conversationId !== Number(event.conversation_id)) {
        return;
      }
      addOrUpdateMessage(transformKnowledgeWritebackEvent(event));
    });
  }, [conversationId, addOrUpdateMessage]);
};

export const useRemoveMessageByMsgId = () => {
  const update = useUpdateMessageList();

  return useCallback(
    (msgId: string) => {
      update((list) => list.filter((message) => message.msg_id !== msgId));
    },
    [update]
  );
};

export const useRemoveMessagesFrom = () => {
  const update = useUpdateMessageList();

  return useCallback(
    (createdAt: number) => {
      update((list) => list.filter((message) => (message.created_at ?? 0) < createdAt));
    },
    [update]
  );
};

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const parseJsonRecord = (value: unknown): Record<string, unknown> | undefined => {
  if (isRecord(value)) return value;
  if (typeof value !== 'string') return undefined;
  try {
    const parsed = JSON.parse(value) as unknown;
    return isRecord(parsed) ? parsed : undefined;
  } catch {
    return undefined;
  }
};

const normalizeTipType = (value: unknown, fallback: IMessageTips['content']['type']) =>
  value === 'success' || value === 'warning' || value === 'error' ? value : fallback;

const normalizePersistedWorkspaceRuntimeError = (
  parsed: Record<string, unknown>,
  message: string
): AgentStreamErrorInfo | undefined => {
  if (parsed.code !== 'WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED') {
    return undefined;
  }

  const details = isRecord(parsed.details) ? parsed.details : undefined;
  const workspacePath = typeof details?.workspace_path === 'string' ? details.workspace_path : undefined;
  if (!workspacePath) {
    return undefined;
  }

  const persistedError = isRecord(parsed.error) ? parsed.error : undefined;
  const detail = typeof persistedError?.detail === 'string' ? persistedError.detail : message;

  return {
    message,
    code: 'WORKSPACE_PATH_EDGE_WHITESPACE_RUNTIME_UNSUPPORTED',
    ownership: 'nomifun',
    detail,
    workspacePath,
    retryable: false,
    feedback_recommended: false,
  };
};

const classifyPersistedSendFailure = (
  parsed: Record<string, unknown>,
  message: string
): AgentStreamErrorInfo | undefined => {
  if (typeof parsed.source !== 'string' && typeof parsed.code !== 'string') {
    return undefined;
  }

  const persistedCode = typeof parsed.code === 'string' ? parsed.code : undefined;
  if (persistedCode === 'BAD_GATEWAY') {
    return {
      message,
      code: 'UNKNOWN_UPSTREAM_ERROR',
      ownership: 'unknown_upstream',
      detail: message,
      retryable: true,
      feedback_recommended: true,
    };
  }

  if (persistedCode === 'INTERNAL_ERROR') {
    return {
      message,
      code: 'NOMIFUN_INTERNAL_ERROR',
      ownership: 'nomifun',
      detail: message,
      retryable: true,
      feedback_recommended: true,
    };
  }

  if (persistedCode?.startsWith('NOMIFUN_')) {
    return { message, code: persistedCode, ownership: 'nomifun', detail: message, retryable: true };
  }
  if (persistedCode?.startsWith('USER_AGENT_')) {
    return { message, code: persistedCode, ownership: 'user_agent', detail: message, retryable: true };
  }
  if (persistedCode?.startsWith('USER_LLM_PROVIDER_')) {
    return {
      message,
      code: persistedCode,
      ownership: 'user_llm_provider',
      detail: message,
      retryable: false,
      feedback_recommended: false,
    };
  }
  if (persistedCode === 'UNKNOWN_UPSTREAM_ERROR') {
    return {
      message,
      code: persistedCode,
      ownership: 'unknown_upstream',
      detail: message,
      retryable: true,
      feedback_recommended: true,
    };
  }

  if (parsed.source === 'send_failed') {
    return {
      message,
      code: 'NOMIFUN_INTERNAL_ERROR',
      ownership: 'nomifun',
      detail: message,
      retryable: true,
      feedback_recommended: true,
    };
  }

  return undefined;
};

const normalizeDbTipsMessage = (msg: TMessage): TMessage => {
  if (msg.type !== 'tips') return msg;
  const parsed = parseJsonRecord(msg.content);
  if (!parsed || typeof parsed.content !== 'string') return msg;

  const existingContent = isRecord(msg.content) ? msg.content : undefined;
  const fallbackType =
    existingContent?.type === 'success' || existingContent?.type === 'warning' || existingContent?.type === 'error'
      ? existingContent.type
      : 'error';
  const tipType = normalizeTipType(parsed.type, fallbackType);
  const structuredError =
    tipType === 'error'
      ? (normalizePersistedWorkspaceRuntimeError(parsed, parsed.content) ??
        normalizeAgentStreamError(parsed.error) ??
        classifyPersistedSendFailure(parsed, parsed.content) ??
        normalizeAgentStreamError({ ...parsed, message: parsed.content }))
      : undefined;

  return {
    ...msg,
    content: {
      content: parsed.content,
      type: tipType,
      ...(structuredError ? { error: structuredError } : {}),
    },
  } as IMessageTips;
};

/**
 * Normalize a message loaded from backend DB: if `content` is a JSON string,
 * parse it and map stored fields to renderer message content.
 */
export function normalizeDbMessage(msg: TMessage): TMessage {
  if (msg.type === 'tips') return normalizeDbTipsMessage(msg);
  if (msg.type !== 'text') return msg;
  const raw = msg.content as unknown;
  if (typeof raw !== 'string') return msg;
  try {
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (typeof parsed.content !== 'string') return msg;
    const knowledgeWriteback = normalizeKnowledgeWritebackState(parsed.knowledge_writeback);
    return {
      ...msg,
      content: {
        content: parsed.content as string,
        ...(knowledgeWriteback ? { knowledge_writeback: knowledgeWriteback } : {}),
        ...(parsed.teammate_message ? { teammateMessage: true } : {}),
        ...(parsed.sender_name ? { senderName: parsed.sender_name as string } : {}),
        ...(parsed.sender_backend ? { senderAgentType: parsed.sender_backend as string } : {}),
        ...(parsed.sender_conversation_id ? { senderConversationId: parsed.sender_conversation_id as number } : {}),
      },
    };
  } catch {
    return msg;
  }
}

/** Initial / per-page window size for keyset (windowed) history loading. */
const HISTORY_WINDOW_SIZE = 60;

/** Keyset cursor for a loaded message: "<created_at_ms>:<id>" (see backend
 *  `parse_message_cursor` / `get_messages_keyset`). */
const messageCursorOf = (m: TMessage): string => `${m.created_at ?? 0}:${m.id}`;

const getFetchedMergeKey = (message: TMessage): string | undefined => {
  if (!message.msg_id) return undefined;
  return `${message.type}:${message.msg_id}`;
};

const getThinkingTextLength = (message: IMessageThinking): number => {
  const content = message.content?.content;
  return typeof content === 'string' ? content.length : 0;
};

const preferThinkingMessageVersion = (
  dbMessage: IMessageThinking,
  streamMessage: IMessageThinking
): IMessageThinking => {
  const dbLength = getThinkingTextLength(dbMessage);
  const streamLength = getThinkingTextLength(streamMessage);
  if (streamLength > dbLength) return streamMessage;
  if (dbLength > streamLength) return dbMessage;
  if (dbMessage.content.status === 'done' && streamMessage.content.status !== 'done') return dbMessage;
  return dbMessage;
};

export const mergeFetchedMessagesForConversation = (
  currentList: TMessage[],
  messages: TMessage[],
  conversationId: number
): TMessage[] => {
  if (!currentList.length) return messages;
  const sameConversation = currentList.filter((m) => m.conversation_id === conversationId);
  if (!sameConversation.length) return messages;

  const dbIds = new Set(messages.map((m) => m.id));
  const dbKeys = new Set(messages.map(getFetchedMergeKey).filter((key): key is string => Boolean(key)));
  const streamingByKey = new Map<string, TMessage>();

  for (const message of sameConversation) {
    const key = getFetchedMergeKey(message);
    if (key && dbKeys.has(key)) {
      streamingByKey.set(key, message);
    }
  }

  const mergedMessages = messages.map((dbMessage) => {
    const key = getFetchedMergeKey(dbMessage);
    const streamMessage = key ? streamingByKey.get(key) : undefined;
    if (!streamMessage) return dbMessage;

    if (dbMessage.type === 'text' && streamMessage.type === 'text') {
      return preferTextMessageVersion(dbMessage, streamMessage);
    }
    if (dbMessage.type === 'thinking' && streamMessage.type === 'thinking') {
      return preferThinkingMessageVersion(dbMessage, streamMessage);
    }
    return dbMessage;
  });

  const streamingOnly = sameConversation.filter((message) => {
    if (dbIds.has(message.id)) return false;
    const key = getFetchedMergeKey(message);
    return !key || !dbKeys.has(key);
  });

  if (!streamingOnly.length && !streamingByKey.size) return messages;
  return [...mergedMessages, ...streamingOnly];
};

/**
 * Loads a conversation's message history into the shared message-list store.
 *
 * Two modes:
 *  - default (legacy): one shot of up to 10000 messages.
 *  - `windowed: true`: keyset pagination — load only the newest
 *    `HISTORY_WINDOW_SIZE` on mount and expose `loadOlder()` to prepend older
 *    windows on scroll-up. Used by the nomi chat surfaces (incl. the companion's
 *    single session, which now also absorbs every IM-channel turn and can grow
 *    without bound) so an enormous transcript never crushes the API/DB or the
 *    DOM. The returned `{ loadOlder, hasMore, loadingOlder }` is consumed by
 *    `MessageList` to drive the scroll-up trigger + a prepend scroll-anchor.
 */
export const useMessageLstCache = (key: number, opts?: { windowed?: boolean }) => {
  const windowed = opts?.windowed ?? false;
  const update = useUpdateMessageList();
  const setLoading = useUpdateMessageListLoading();
  const [hasMore, setHasMore] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  // Oldest message currently loaded (drives the next "load older" cursor); ref
  // mirrors so the event-driven callbacks read the latest without re-binding.
  const oldestCursorRef = useRef<string | null>(null);
  const hasMoreRef = useRef(false);
  const loadingOlderRef = useRef(false);

  // Merge a freshly fetched DB page (newest window or full list) with any
  // in-flight streaming messages for this conversation. During streaming the DB
  // may hold an older snapshot (2000ms save debounce), so we keep whichever
  // version has more content and append streaming-only rows at the tail.
  const mergeIntoList = useCallback(
    (messages: TMessage[]) => {
      update((currentList) => {
        return mergeFetchedMessagesForConversation(currentList, messages, key);
      });
    },
    [key, update]
  );

  const loadMessages = useCallback(async (): Promise<TMessage[]> => {
    const result = await ipcBridge.database.getConversationMessages.invoke(
      windowed
        ? { conversation_id: key, cursor: '', page_size: HISTORY_WINDOW_SIZE, content_mode: 'compact' }
        : { conversation_id: key, page: 0, page_size: 10000, content_mode: 'compact' }
    );
    const messages = result?.items?.map(normalizeDbMessage);
    if (windowed) {
      hasMoreRef.current = Boolean(result?.has_more);
      setHasMore(hasMoreRef.current);
      // Keyset path returns the window oldest-first, so messages[0] is the oldest.
      oldestCursorRef.current = messages && messages.length ? messageCursorOf(messages[0]) : null;
    }
    if (messages && Array.isArray(messages)) {
      mergeIntoList(messages);
      return messages;
    }
    return [];
  }, [key, mergeIntoList, windowed]);

  // Prepend the next older window (scroll-up). Older rows never overlap the live
  // streaming tail, so an id-dedup prepend suffices (no content merge needed).
  const loadOlder = useCallback(async (): Promise<void> => {
    if (!windowed || loadingOlderRef.current || !hasMoreRef.current) return;
    const cursor = oldestCursorRef.current;
    if (!cursor) return;
    loadingOlderRef.current = true;
    setLoadingOlder(true);
    try {
      const result = await ipcBridge.database.getConversationMessages.invoke({
        conversation_id: key,
        cursor,
        page_size: HISTORY_WINDOW_SIZE,
        content_mode: 'compact',
      });
      const older = result?.items?.map(normalizeDbMessage) ?? [];
      hasMoreRef.current = Boolean(result?.has_more);
      setHasMore(hasMoreRef.current);
      if (older.length) {
        oldestCursorRef.current = messageCursorOf(older[0]);
        update((currentList) => {
          const existingIds = new Set(currentList.map((m) => m.id));
          const fresh = older.filter((m) => !existingIds.has(m.id));
          return fresh.length ? [...fresh, ...currentList] : currentList;
        });
      }
    } catch (error) {
      console.error('[useMessageLstCache] Failed to load older messages:', error);
    } finally {
      loadingOlderRef.current = false;
      setLoadingOlder(false);
    }
  }, [key, update, windowed]);

  useEffect(() => {
    if (!key) return;
    // Reset windowed paging state on conversation switch.
    oldestCursorRef.current = null;
    hasMoreRef.current = false;
    setHasMore(false);
    let cancelled = false;
    setLoading(true);
    void loadMessages()
      .catch((error) => {
        console.error('[useMessageLstCache] Failed to load messages from database:', error);
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [key, loadMessages, setLoading]);

  return { loadOlder, hasMore, loadingOlder };
};

export {
  MessageListLoadingProvider,
  MessageListProvider,
  useMessageList,
  useMessageListLoading,
  useUpdateMessageList,
};
