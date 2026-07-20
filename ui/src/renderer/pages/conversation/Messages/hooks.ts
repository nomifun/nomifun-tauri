/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import {
  parseConversationId,
  parseCronJobId,
  parseMessageId,
  type ConversationId,
  type MessageId,
} from '@/common/types/ids';
import { ipcBridge } from '@/common';
import type {
  AgentStreamErrorInfo,
  IMessageText,
  IMessageThinking,
  IMessageTips,
  TMessage,
} from '@/common/chat/chatLib';
import { toDisplayText } from '@/common/chat/displayText';
import {
  composeMessage,
  mergeAcpToolCallContent,
  mergeToolCallContent,
  mergeTextMessageContent,
  normalizeAcpToolCallContent,
  normalizeKnowledgeWritebackState,
  normalizeToolCallContent,
  normalizeToolGroupContent,
  normalizeWireAgentMessageMetadata,
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
  call_idIndex: Map<string, number>; // turn + tool_call.call_id -> index
  tool_call_idIndex: Map<string, number>; // turn + acp tool_call_id -> index
  permission_call_idIndex: Map<string, number>; // permission.content.call_id -> index
}

const getToolLifecycleKey = (message: TMessage, callId: string): string => {
  const contentTurnId =
    message.content && typeof message.content === 'object' && 'turn_id' in message.content
      ? toDisplayText((message.content as { turn_id?: unknown }).turn_id)
      : '';
  const turnId = message.turn_id || contentTurnId || message.msg_id || message.id;
  return `${turnId}:${callId}`;
};

function getMessageIndexKey(message: TMessage): string | undefined {
  if (!message.msg_id) return undefined;
  if (message.type === 'thinking') return `thinking:${message.msg_id}`;
  if (message.type === 'agent_status') {
    const backend = typeof message.content?.backend === 'string' ? message.content.backend : 'agent';
    return `agent_status:${message.msg_id}:${backend}`;
  }
  // A msg_id identifies the owning stream segment, not a renderer row. Text,
  // tips, plans and tool/status events can legitimately share it. Keeping a
  // type namespace prevents a terminal error from replacing the successful
  // assistant text that preceded it (and vice versa).
  return `${message.type}:${message.msg_id}`;
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
      call_idIndex.set(getToolLifecycleKey(msg, msg.content.call_id), i);
    }
    if (msg.type === 'acp_tool_call' && msg.content?.update?.tool_call_id) {
      tool_call_idIndex.set(getToolLifecycleKey(msg, msg.content.update.tool_call_id), i);
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
    const existingIdx = index.msgIdIndex.get(getMessageIndexKey(message)!);
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
    index.msgIdIndex.set(getMessageIndexKey(message)!, newIdx);
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
    const lifecycleKey = getToolLifecycleKey(message, message.content.call_id);
    const existingIdx = index.call_idIndex.get(lifecycleKey);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'tool_call') {
        const newList = list.slice();
        const merged = mergeToolCallContent(existingMsg.content, message.content);
        newList[existingIdx] = {
          ...existingMsg,
          ...message,
          // Lifecycle updates must not move a process step in transcript
          // order or churn its React identity. Later frames only update its
          // state/content; the first frame owns its stable envelope.
          id: existingMsg.id,
          msg_id: existingMsg.msg_id ?? message.msg_id,
          turn_id: message.turn_id ?? existingMsg.turn_id,
          created_at: existingMsg.created_at ?? message.created_at,
          content: merged,
        };
        return newList;
      }
    }
    // 未找到，添加新消息并更新索引
    const newIdx = list.length;
    index.call_idIndex.set(lifecycleKey, newIdx);
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
    return list.concat(message);
  }

  // acp_tool_call: use tool_call_idIndex for fast lookup
  if (message.type === 'acp_tool_call' && message.content?.update?.tool_call_id) {
    const lifecycleKey = getToolLifecycleKey(message, message.content.update.tool_call_id);
    const existingIdx = index.tool_call_idIndex.get(lifecycleKey);
    if (existingIdx !== undefined && existingIdx < list.length) {
      const existingMsg = list[existingIdx];
      if (existingMsg.type === 'acp_tool_call') {
        const newList = list.slice();
        const merged = mergeAcpToolCallContent(existingMsg.content, message.content);
        newList[existingIdx] = {
          ...existingMsg,
          ...message,
          id: existingMsg.id,
          msg_id: existingMsg.msg_id ?? message.msg_id,
          turn_id: message.turn_id ?? existingMsg.turn_id,
          created_at: existingMsg.created_at ?? message.created_at,
          content: merged,
        };
        return newList;
      }
    }
    // 未找到，添加新消息并更新索引
    const newIdx = list.length;
    index.tool_call_idIndex.set(lifecycleKey, newIdx);
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
    const textKey = getMessageIndexKey(message)!;
    const existingIdx = index.msgIdIndex.get(textKey);
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
        // Complete inter-Agent messages are not streaming chunks — skip if already present.
        if ((message.content as { agentMessage?: boolean })?.agentMessage) {
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
    index.msgIdIndex.set(textKey, newIdx);
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
            turn_id: message.turn_id ?? existingMsg.turn_id,
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

  // plan message: update content and move to end of list. Prefer exact msg_id,
  // then fall back to the plan session id so a later turn can refresh the same
  // visible checklist even when the backend minted a new message id.
  if (message.type === 'plan') {
    let existingIdx = message.msg_id ? index.msgIdIndex.get(getMessageIndexKey(message)!) : undefined;
    if (existingIdx !== undefined && list[existingIdx]?.type !== 'plan') {
      existingIdx = undefined;
    }
    if (existingIdx === undefined) {
      const sessionId = message.content.session_id;
      for (let i = list.length - 1; i >= 0; i--) {
        const candidate = list[i];
        if (candidate.type === 'plan' && candidate.content.session_id === sessionId) {
          existingIdx = i;
          break;
        }
      }
    }

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
    const msgIndexKey = getMessageIndexKey(message);
    if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
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

type PendingMessageUpdate = { message: TMessage; add: boolean };
type PendingMessageUpdateRef = { current: PendingMessageUpdate[] };
type MessageListFunctionalUpdate = (updater: (list: TMessage[]) => TMessage[]) => void;

/**
 * Atomically drains one renderer message batch into the shared list.
 *
 * Keeping the drain synchronous is important for effect cleanup: a stream event
 * can arrive in the same task as a route/conversation switch, before the normal
 * zero-delay timer runs. Clearing that timer without consuming its queue drops
 * the event. The ref is swapped before invoking React's updater so a re-entrant
 * event is placed in a new batch and can never be applied twice.
 */
export function drainPendingMessageUpdates(
  pendingRef: PendingMessageUpdateRef,
  update: MessageListFunctionalUpdate
): boolean {
  const pending = pendingRef.current;
  if (!pending.length) return false;
  pendingRef.current = [];

  update((list) => {
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
        const msg = item.message;
        const newIdx = newList.length;
        const msgIndexKey = getMessageIndexKey(msg);
        if (msgIndexKey) index.msgIdIndex.set(msgIndexKey, newIdx);
        if (msg.type === 'tool_call' && msg.content?.call_id) {
          index.call_idIndex.set(getToolLifecycleKey(msg, msg.content.call_id), newIdx);
        }
        if (msg.type === 'acp_tool_call' && msg.content?.update?.tool_call_id) {
          index.tool_call_idIndex.set(getToolLifecycleKey(msg, msg.content.update.tool_call_id), newIdx);
        }
        if (msg.type === 'permission' && msg.content?.call_id) {
          index.permission_call_idIndex.set(msg.content.call_id, newIdx);
        }
        newList = newList.concat(msg);
      } else {
        newList = composeMessageWithIndex(item.message, newList, index);
      }

      while (beforeUpdateMessageListStack.length) {
        newList = beforeUpdateMessageListStack.shift()!(newList);
      }
    }
    return newList;
  });

  return true;
}

export const useAddOrUpdateMessage = () => {
  const update = useUpdateMessageList();
  const pendingRef = useRef<PendingMessageUpdate[]>([]);
  const rafRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(() => {
    rafRef.current = null;
    drainPendingMessageUpdates(pendingRef, update);
  }, [update]);

  useEffect(() => {
    return () => {
      if (rafRef.current !== null) {
        clearTimeout(rafRef.current);
        rafRef.current = null;
      }
      // Navigation can race the zero-delay batch timer. Drain synchronously so
      // cleanup never discards the final user/stream message in that task.
      flush();
    };
  }, [flush]);

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

export const useKnowledgeWritebackEvents = (conversationId: ConversationId | undefined) => {
  const addOrUpdateMessage = useAddOrUpdateMessage();

  useEffect(() => {
    if (!conversationId) return;
    return ipcBridge.conversation.knowledgeWriteback.on((event) => {
      if (conversationId !== event.conversation_id) {
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

const parseJsonArray = (value: unknown): unknown[] | undefined => {
  if (Array.isArray(value)) return value;
  if (typeof value !== 'string') return undefined;
  try {
    const parsed = JSON.parse(value) as unknown;
    return Array.isArray(parsed) ? parsed : undefined;
  } catch {
    return undefined;
  }
};

const normalizeTipType = (value: unknown, fallback: IMessageTips['content']['type']) =>
  value === 'success' || value === 'warning' || value === 'error' ? value : fallback;

const normalizePersistedTurnId = (value: unknown): MessageId | undefined => {
  if (typeof value !== 'string') return undefined;
  try {
    return parseMessageId(value);
  } catch {
    return undefined;
  }
};

const resolvePersistedTurnId = (
  msg: TMessage,
  parsed: Record<string, unknown>
): MessageId | undefined =>
  normalizePersistedTurnId(msg.turn_id) ?? normalizePersistedTurnId(parsed.turn_id);

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
  const turnId = resolvePersistedTurnId(msg, parsed);
  const structuredError =
    tipType === 'error'
      ? (normalizePersistedWorkspaceRuntimeError(parsed, parsed.content) ??
        normalizeAgentStreamError(parsed.error) ??
        classifyPersistedSendFailure(parsed, parsed.content) ??
        normalizeAgentStreamError({ ...parsed, message: parsed.content }))
      : undefined;

  return {
    ...msg,
    turn_id: turnId,
    content: {
      content: parsed.content,
      type: tipType,
      ...(structuredError ? { error: structuredError } : {}),
    },
  } as IMessageTips;
};

const normalizeDecodedTextMetadata = (parsed: Record<string, unknown>): Partial<IMessageText['content']> => {
  const metadata: Partial<IMessageText['content']> = {
    ...(parsed.replace === true ? { replace: true } : {}),
    ...(parsed.agentMessage === true ? { agentMessage: true } : {}),
    ...(typeof parsed.senderName === 'string' ? { senderName: parsed.senderName } : {}),
    ...(typeof parsed.senderAgentType === 'string' ? { senderAgentType: parsed.senderAgentType } : {}),
  };

  if (typeof parsed.senderConversationId === 'string') {
    try {
      metadata.senderConversationId = parseConversationId(parsed.senderConversationId);
    } catch {
      // Ignore malformed persisted collaboration metadata without dropping the
      // otherwise valid message body.
    }
  }

  const cronMeta = isRecord(parsed.cronMeta) ? parsed.cronMeta : undefined;
  if (
    cronMeta?.source === 'cron' &&
    typeof cronMeta.cron_job_id === 'string' &&
    typeof cronMeta.cron_job_name === 'string' &&
    typeof cronMeta.triggered_at === 'number'
  ) {
    try {
      metadata.cronMeta = {
        source: 'cron',
        cron_job_id: parseCronJobId(cronMeta.cron_job_id),
        cron_job_name: cronMeta.cron_job_name,
        triggered_at: cronMeta.triggered_at,
      };
    } catch {
      // Same trust boundary as senderConversationId above.
    }
  }

  return metadata;
};

/**
 * Normalize a message loaded from backend DB. `content` may be a JSON string
 * or an object decoded by the transport mapper; both shapes are projected into
 * renderer content and their persisted ownership metadata is restored.
 */
export function normalizeDbMessage(msg: TMessage): TMessage {
  if (msg.type === 'tips') return normalizeDbTipsMessage(msg);
  if (msg.type === 'tool_call') {
    const parsed = parseJsonRecord(msg.content) ?? {};
    const content = normalizeToolCallContent(parsed, msg.status ?? null);
    return {
      ...msg,
      turn_id: resolvePersistedTurnId(msg, parsed),
      content,
      status: content.status === 'error' ? 'error' : msg.status,
    };
  }
  if (msg.type === 'acp_tool_call') {
    const parsed = parseJsonRecord(msg.content) ?? {};
    const content = normalizeAcpToolCallContent(parsed, msg.status ?? null);
    return {
      ...msg,
      turn_id: resolvePersistedTurnId(msg, parsed),
      content,
      status: content.update?.status === 'failed' ? 'error' : msg.status,
    };
  }
  if (msg.type === 'tool_group') {
    const content = normalizeToolGroupContent(parseJsonArray(msg.content) ?? []);
    return {
      ...msg,
      content,
      status: content.some((item) => item.status === 'Error') ? 'error' : msg.status,
    };
  }
  if (msg.type !== 'text') return msg;
  // Depending on the IPC/REST mapper, JSON DB content can arrive either as its
  // original string or as an already-decoded object. Both shapes must restore
  // the top-level owning turn; leaving it nested makes history/live hydration
  // split one ACP request between the user message id and the root turn id.
  const parsed = parseJsonRecord(msg.content);
  if (!parsed || typeof parsed.content !== 'string') return msg;
  const knowledgeWriteback = normalizeKnowledgeWritebackState(parsed.knowledge_writeback);
  return {
    ...msg,
    turn_id: resolvePersistedTurnId(msg, parsed),
    content: {
      content: parsed.content,
      ...normalizeDecodedTextMetadata(parsed),
      ...(knowledgeWriteback ? { knowledge_writeback: knowledgeWriteback } : {}),
      ...normalizeWireAgentMessageMetadata(parsed),
    },
  };
}

/** Initial / per-page window size for keyset (windowed) history loading. */
const HISTORY_WINDOW_SIZE = 60;

/** Keyset cursor for a loaded message: "<created_at_ms>:<id>" (see backend
 *  `parse_message_cursor` / `get_messages_keyset`). */
const messageCursorOf = (m: TMessage): string => `${m.created_at ?? 0}:${m.id}`;

const getFetchedMergeKey = (message: TMessage): string | undefined => {
  if (!message.msg_id) return undefined;
  if (message.type === 'tool_call' && message.content?.call_id) {
    return `tool_call:${getToolLifecycleKey(message, message.content.call_id)}`;
  }
  if (message.type === 'acp_tool_call' && message.content?.update?.tool_call_id) {
    return `acp_tool_call:${getToolLifecycleKey(message, message.content.update.tool_call_id)}`;
  }
  return `${message.type}:${message.msg_id}`;
};

const compareTranscriptOrder = (left: TMessage, right: TMessage): number => {
  const leftCreatedAt = left.created_at ?? Number.MAX_SAFE_INTEGER;
  const rightCreatedAt = right.created_at ?? Number.MAX_SAFE_INTEGER;
  if (leftCreatedAt !== rightCreatedAt) return leftCreatedAt - rightCreatedAt;
  // IDs are canonical ASCII. Avoid localeCompare so ordering is identical
  // across Windows/macOS/Linux regardless of the host ICU locale.
  return left.id === right.id ? 0 : left.id < right.id ? -1 : 1;
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

const withFetchedCanonicalIdentity = <T extends TMessage>(dbMessage: T, preferred: T): T =>
  ({
    ...preferred,
    // The fetched row owns durable identity and chronological placement even
    // when a longer/live payload wins for display content.
    id: dbMessage.id,
    msg_id: dbMessage.msg_id ?? preferred.msg_id,
    conversation_id: dbMessage.conversation_id,
    created_at: dbMessage.created_at ?? preferred.created_at,
    turn_id: dbMessage.turn_id ?? preferred.turn_id,
    status: dbMessage.status ?? preferred.status,
  }) as T;

export const mergeFetchedMessagesForConversation = (
  currentList: TMessage[],
  messages: TMessage[],
  conversationId: ConversationId
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
      return withFetchedCanonicalIdentity(dbMessage, preferTextMessageVersion(dbMessage, streamMessage));
    }
    if (dbMessage.type === 'thinking' && streamMessage.type === 'thinking') {
      return withFetchedCanonicalIdentity(dbMessage, preferThinkingMessageVersion(dbMessage, streamMessage));
    }
    if (dbMessage.type === 'tool_call' && streamMessage.type === 'tool_call') {
      const content = mergeToolCallContent(dbMessage.content, streamMessage.content);
      return {
        ...dbMessage,
        ...streamMessage,
        id: dbMessage.id,
        msg_id: dbMessage.msg_id ?? streamMessage.msg_id,
        conversation_id: dbMessage.conversation_id,
        created_at: dbMessage.created_at ?? streamMessage.created_at,
        turn_id: dbMessage.turn_id ?? streamMessage.turn_id,
        content,
        status: content.status === 'error' ? 'error' : dbMessage.status,
      };
    }
    if (dbMessage.type === 'acp_tool_call' && streamMessage.type === 'acp_tool_call') {
      const content = mergeAcpToolCallContent(dbMessage.content, streamMessage.content);
      return {
        ...dbMessage,
        ...streamMessage,
        id: dbMessage.id,
        msg_id: dbMessage.msg_id ?? streamMessage.msg_id,
        conversation_id: dbMessage.conversation_id,
        created_at: dbMessage.created_at ?? streamMessage.created_at,
        turn_id: dbMessage.turn_id ?? streamMessage.turn_id,
        content,
        status: content.update.status === 'failed' ? 'error' : dbMessage.status,
      };
    }
    return dbMessage;
  });

  const streamingOnly = sameConversation.filter((message) => {
    if (dbIds.has(message.id)) return false;
    const key = getFetchedMergeKey(message);
    if (key && dbKeys.has(key)) return false;

    // This is a chronological union, not an append-only streaming tail. Rows
    // from older keyset pages and genuinely in-flight rows both remain visible;
    // canonical IDs/keys above remove their persisted live counterparts.
    return true;
  });

  if (!streamingOnly.length && !streamingByKey.size) return messages;
  return [...mergedMessages, ...streamingOnly].sort(compareTranscriptOrder);
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
export const useMessageLstCache = (key: ConversationId, opts?: { windowed?: boolean }) => {
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
  const newestLoadSequenceRef = useRef(0);
  const activeConversationRef = useRef<ConversationId>(key);

  // Providers are shared by the mounted chat surface. Invalidate outstanding
  // fetches synchronously when routing to another conversation so a slower old
  // request can never replace the new conversation's transcript.
  if (activeConversationRef.current !== key) {
    newestLoadSequenceRef.current += 1;
    activeConversationRef.current = key;
  }

  // Merge a freshly fetched DB page (newest window or full list) with any
  // in-flight streaming messages for this conversation. During streaming the DB
  // may hold an older snapshot (2000ms save debounce), so we keep whichever
  // version has more content and build a stable chronological union.
  const mergeIntoList = useCallback(
    (messages: TMessage[]) => {
      update((currentList) => {
        return mergeFetchedMessagesForConversation(currentList, messages, key);
      });
    },
    [key, update]
  );

  const loadMessages = useCallback(async (): Promise<TMessage[]> => {
    const loadSequence = newestLoadSequenceRef.current + 1;
    newestLoadSequenceRef.current = loadSequence;
    const result = await ipcBridge.database.getConversationMessages.invoke(
      windowed
        ? { conversation_id: key, cursor: '', page_size: HISTORY_WINDOW_SIZE, content_mode: 'compact' }
        : { conversation_id: key, page: 0, page_size: 10000, content_mode: 'compact' }
    );
    const messages = result?.items?.map(normalizeDbMessage);
    if (
      activeConversationRef.current !== key ||
      newestLoadSequenceRef.current !== loadSequence
    ) {
      return [];
    }
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
      if (activeConversationRef.current !== key) return;
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
      newestLoadSequenceRef.current += 1;
    };
  }, [key, loadMessages, setLoading]);

  // Plan rows are finalized in the database when the authoritative turn
  // completes, but that status-only write does not need another plan stream
  // fragment. Refresh the current window so terminal plan state is reflected
  // immediately instead of waiting for a remount/manual history refresh.
  useEffect(() => {
    if (!key) return;
    return ipcBridge.conversation.turnCompleted.on((event) => {
      if (event.conversation_id !== key || event.runtime.is_processing) return;
      void loadMessages().catch((error) => {
        console.warn('[useMessageLstCache] Failed to refresh terminal message state:', error);
      });
    });
  }, [key, loadMessages]);

  return { loadOlder, hasMore, loadingOlder };
};

export {
  MessageListLoadingProvider,
  MessageListProvider,
  useMessageList,
  useMessageListLoading,
  useUpdateMessageList,
};
