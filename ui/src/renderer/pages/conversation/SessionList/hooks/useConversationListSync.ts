/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { ConversationId, MessageId } from '@/common/types/ids';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import {
  getConversationRuntimeAuthority,
  isCompleteMessageProjection,
} from '@/renderer/pages/conversation/utils/conversationRuntime';
import { addEventListener } from '@/renderer/utils/emitter';
import { useCallback, useEffect, useSyncExternalStore } from 'react';
import { isAuthoritativeCompletionRuntimeIdle } from '../../platforms/authoritativeTurnLifecyclePolicy';

import { isOrdinaryWorkConversation } from './conversationListFilter';

/**
 * Whitelist of message types that indicate content generation is in progress.
 * Only these types should trigger the sidebar loading spinner.
 * Using a whitelist (instead of a blacklist) prevents unknown/internal message
 * types (e.g. slash_commands_updated, acp_context_usage) from falsely
 * triggering the generating state.
 */
const isPreparingAgentStatus = (data: unknown): boolean => {
  if (!data || typeof data !== 'object') {
    return false;
  }

  return (data as { status?: string }).status === 'preparing';
};

export const isGeneratingStreamMessage = (message: {
  type: string;
  data: unknown;
  stream_complete?: boolean;
}): boolean => {
  // Finalized assistant projections (for example an Agent Execution terminal
  // report) use message.stream only as a realtime delivery channel. They do
  // not start a model turn and deliberately have no later terminal event.
  if (isCompleteMessageProjection(message)) {
    return false;
  }

  if (message.type === 'agent_status') {
    return isPreparingAgentStatus(message.data);
  }

  const { type } = message;
  return (
    type === 'content' ||
    type === 'start' ||
    type === 'thought' ||
    type === 'thinking' ||
    type === 'tool_group' ||
    type === 'acp_tool_call' ||
    type === 'acp_permission' ||
    type === 'permission' ||
    type === 'plan'
  );
};

const isTerminalTurnState = (state: string): boolean => {
  return state === 'ai_waiting_input' || state === 'error' || state === 'stopped';
};

export const getExactSidebarActiveTurnId = (
  conversation?: Pick<TChatConversation, 'runtime' | 'status'> | null
): MessageId | null =>
  getConversationRuntimeAuthority(conversation) === 'processing'
    ? (conversation?.runtime?.active_turn_id ?? null)
    : null;

export const shouldAcceptSidebarTurnStart = ({
  turnId,
  eventRuntimeIsProcessing,
  eventActiveTurnId,
  conversation,
}: {
  turnId: MessageId;
  eventRuntimeIsProcessing: boolean;
  eventActiveTurnId?: MessageId;
  conversation?: Pick<TChatConversation, 'runtime' | 'status'> | null;
}): boolean =>
  eventRuntimeIsProcessing &&
  eventActiveTurnId === turnId &&
  getExactSidebarActiveTurnId(conversation) === turnId;

export const shouldApplySidebarStreamActivity = ({
  messageTurnId,
  activeTurnId,
}: {
  messageTurnId?: MessageId;
  activeTurnId?: MessageId;
}): boolean =>
  messageTurnId != null &&
  activeTurnId != null &&
  messageTurnId === activeTurnId;

export const shouldAcceptSidebarTurnCompletion = ({
  completedTurnId,
  activeTurnId,
  eventRuntimeIsProcessing,
  eventActiveTurnId,
  conversation,
}: {
  completedTurnId?: MessageId;
  activeTurnId?: MessageId;
  eventRuntimeIsProcessing: boolean;
  eventActiveTurnId?: MessageId;
  conversation?: Pick<TChatConversation, 'runtime' | 'status'> | null;
}): boolean => {
  if (
    eventRuntimeIsProcessing ||
    eventActiveTurnId != null ||
    getConversationRuntimeAuthority(conversation) !== 'idle'
  ) {
    return false;
  }

  return !activeTurnId || !completedTurnId || activeTurnId === completedTurnId;
};

type ConversationListSyncSnapshot = {
  conversations: TChatConversation[];
  generatingConversationIds: Set<ConversationId>;
  completionUnreadConversationIds: Set<ConversationId>;
};

const listeners = new Set<() => void>();

let isStoreInitialized = false;
let conversationsState: TChatConversation[] = [];
let generatingConversationIdsState = new Set<ConversationId>();
let completionUnreadConversationIdsState = new Set<ConversationId>();
let conversation_idsState = new Set<ConversationId>();
let activeTurnIdsState = new Map<ConversationId, MessageId>();
let activeConversationIdState: ConversationId | null = null;
let snapshotState: ConversationListSyncSnapshot = {
  conversations: conversationsState,
  generatingConversationIds: generatingConversationIdsState,
  completionUnreadConversationIds: completionUnreadConversationIdsState,
};

const emitStoreChange = () => {
  snapshotState = {
    conversations: conversationsState,
    generatingConversationIds: generatingConversationIdsState,
    completionUnreadConversationIds: completionUnreadConversationIdsState,
  };
  listeners.forEach((listener) => listener());
};

const subscribeConversationListSync = (listener: () => void) => {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
};

const getConversationListSyncSnapshot = (): ConversationListSyncSnapshot => snapshotState;

const refreshConversations = () => {
  void ipcBridge.database.getUserConversations
    .invoke({ limit: 10000 })
    .then((result) => {
      const items = result?.items;
      if (items && Array.isArray(items)) {
        const filteredData = items.filter((conv) => {
          // Legacy rows from the pre-provider-probe health check flow are hidden
          // from normal history. New health checks must not create conversations.
          // Companion conversations — the desktop bubble, the chat tab, AND every
          // IM-channel turn — all share ONE per-companion session that lives in
          // 桌面伙伴→伙伴→聊天, never in this work conversation list. Hide every
          // companion row, identified by any companion marker in `extra`
          // (companionSession / companionId / channelPlatform). The previous
          // carve-out that KEPT channel-sourced companion sessions visible here
          // is exactly what leaked IM chats into the work space — it is removed,
          // which also fixes Slack/Discord (source==='nomifun') being mis-bucketed.
          return isOrdinaryWorkConversation(conv);
        });
        conversationsState = filteredData;
        for (const conversation of items) {
          const activeTurnId = getExactSidebarActiveTurnId(conversation);
          if (activeTurnId) {
            activeTurnIdsState.set(conversation.id, activeTurnId);
          } else if (conversation.status === 'finished') {
            activeTurnIdsState.delete(conversation.id);
            clearGenerating(conversation.id);
          }
        }
        // Use ALL conversation IDs (including legacy health-check rows) so the
        // responseStream listener recognises them as known and doesn't
        // trigger an infinite refreshConversations loop.
        conversation_idsState = new Set(items.map((conversation) => conversation.id));
        emitStoreChange();
        return;
      }

      conversationsState = [];
      conversation_idsState = new Set();
      activeTurnIdsState = new Map();
      generatingConversationIdsState = new Set();
      emitStoreChange();
    })
    .catch((error) => {
      console.error('[SessionList] Failed to load conversations:', error);
      conversationsState = [];
      conversation_idsState = new Set();
      activeTurnIdsState = new Map();
      generatingConversationIdsState = new Set();
      emitStoreChange();
    });
};

const markGenerating = (conversation_id: ConversationId) => {
  if (generatingConversationIdsState.has(conversation_id)) {
    return;
  }

  generatingConversationIdsState = new Set(generatingConversationIdsState).add(conversation_id);
  emitStoreChange();
};

const clearGenerating = (conversation_id: ConversationId) => {
  if (!generatingConversationIdsState.has(conversation_id)) {
    return;
  }

  const next = new Set(generatingConversationIdsState);
  next.delete(conversation_id);
  generatingConversationIdsState = next;
  emitStoreChange();
};

const markCompletionUnread = (conversation_id: ConversationId) => {
  if (completionUnreadConversationIdsState.has(conversation_id)) {
    return;
  }

  completionUnreadConversationIdsState = new Set(completionUnreadConversationIdsState).add(conversation_id);
  emitStoreChange();
};

const clearCompletionUnreadState = (conversation_id: ConversationId) => {
  if (!completionUnreadConversationIdsState.has(conversation_id)) {
    return;
  }

  const next = new Set(completionUnreadConversationIdsState);
  next.delete(conversation_id);
  completionUnreadConversationIdsState = next;
  emitStoreChange();
};

const setActiveConversationState = (conversation_id: ConversationId | null) => {
  activeConversationIdState = conversation_id;
};

const initializeConversationListSyncStore = () => {
  if (isStoreInitialized) {
    return;
  }

  isStoreInitialized = true;
  refreshConversations();

  addEventListener('chat.history.refresh', refreshConversations);
  ipcBridge.conversation.listChanged.on((event) => {
    if (event.action === 'deleted') {
      activeTurnIdsState.delete(event.conversation_id);
      clearGenerating(event.conversation_id);
      clearCompletionUnreadState(event.conversation_id);
    }
    refreshConversations();
  });
  ipcBridge.conversation.turnStarted.on((event) => {
    if (
      !event.runtime.is_processing ||
      event.runtime.active_turn_id !== event.turn_id
    ) {
      return;
    }

    const observedRoot = activeTurnIdsState.get(event.conversation_id);
    void getConversationOrNull(event.conversation_id)
      .then((conversation) => {
        if (
          !shouldAcceptSidebarTurnStart({
            turnId: event.turn_id,
            eventRuntimeIsProcessing: event.runtime.is_processing,
            eventActiveTurnId: event.runtime.active_turn_id,
            conversation,
          })
        ) {
          return;
        }

        const currentRoot = activeTurnIdsState.get(event.conversation_id);
        if (currentRoot !== observedRoot && currentRoot !== event.turn_id) {
          return;
        }

        activeTurnIdsState.set(event.conversation_id, event.turn_id);
        markGenerating(event.conversation_id);
        refreshConversations();
      })
      .catch((error) => {
        console.warn('[SessionList] Failed to verify turn.started authority:', error);
      });
  });
  ipcBridge.conversation.responseStream.on((message) => {
    const conversation_id = message.conversation_id;
    if (!conversation_id) {
      return;
    }

    if (!conversation_idsState.has(conversation_id)) {
      refreshConversations();
    }

    if (
      !shouldApplySidebarStreamActivity({
        messageTurnId: message.turn_id,
        activeTurnId: activeTurnIdsState.get(conversation_id),
      })
    ) {
      return;
    }

    if (isGeneratingStreamMessage(message)) {
      markGenerating(conversation_id);
    }
  });
  ipcBridge.conversation.turnCompleted.on((event) => {
    if (!isAuthoritativeCompletionRuntimeIdle(event.runtime)) return;

    const observedRoot = activeTurnIdsState.get(event.conversation_id);
    if (observedRoot && event.turn_id && observedRoot !== event.turn_id) return;

    void getConversationOrNull(event.conversation_id)
      .then((conversation) => {
        if (
          activeTurnIdsState.get(event.conversation_id) !== observedRoot ||
          !shouldAcceptSidebarTurnCompletion({
            completedTurnId: event.turn_id,
            activeTurnId: observedRoot,
            eventRuntimeIsProcessing: event.runtime.is_processing,
            eventActiveTurnId: event.runtime.active_turn_id,
            conversation,
          })
        ) {
          return;
        }

        const wasGenerating = generatingConversationIdsState.has(event.conversation_id);
        activeTurnIdsState.delete(event.conversation_id);
        if (
          wasGenerating &&
          isTerminalTurnState(event.state) &&
          activeConversationIdState !== event.conversation_id
        ) {
          markCompletionUnread(event.conversation_id);
        }
        clearGenerating(event.conversation_id);
        refreshConversations();
      })
      .catch((error) => {
        console.warn('[SessionList] Failed to verify turn.completed authority:', error);
      });
  });
};

export const useConversationListSync = () => {
  useEffect(() => {
    initializeConversationListSyncStore();
  }, []);

  const { conversations, generatingConversationIds, completionUnreadConversationIds } = useSyncExternalStore(
    subscribeConversationListSync,
    getConversationListSyncSnapshot,
    getConversationListSyncSnapshot
  );

  const clearCompletionUnread = useCallback((conversation_id: ConversationId) => {
    clearCompletionUnreadState(conversation_id);
  }, []);

  const setActiveConversation = useCallback((conversation_id: ConversationId | null) => {
    setActiveConversationState(conversation_id);
  }, []);

  const isConversationGenerating = useCallback(
    (conversation_id: ConversationId) => {
      return generatingConversationIds.has(conversation_id);
    },
    [generatingConversationIds]
  );

  const hasCompletionUnread = useCallback(
    (conversation_id: ConversationId) => {
      return completionUnreadConversationIds.has(conversation_id);
    },
    [completionUnreadConversationIds]
  );

  return {
    conversations,
    isConversationGenerating,
    hasCompletionUnread,
    clearCompletionUnread,
    setActiveConversation,
  };
};
