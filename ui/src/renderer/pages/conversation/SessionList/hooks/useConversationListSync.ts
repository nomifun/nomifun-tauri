/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import { addEventListener } from '@/renderer/utils/emitter';
import { useCallback, useEffect, useSyncExternalStore } from 'react';

/**
 * Whitelist of message types that indicate content generation is in progress.
 * Only these types should trigger the sidebar loading spinner.
 * Using a whitelist (instead of a blacklist) prevents unknown/internal message
 * types (e.g. slash_commands_updated, acp_context_usage) from falsely
 * triggering the generating state.
 */
const isGeneratingStreamMessage = (type: string): boolean => {
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

const isTerminalAgentStatus = (data: unknown): boolean => {
  if (!data || typeof data !== 'object') {
    return false;
  }

  const { status } = data as { status?: string };
  return status === 'error' || status === 'disconnected';
};

const isTerminalStreamMessage = (message: { type: string; data: unknown }): boolean => {
  return (
    message.type === 'finish' ||
    message.type === 'error' ||
    (message.type === 'agent_status' && isTerminalAgentStatus(message.data))
  );
};

const isTerminalTurnState = (state: string): boolean => {
  return state === 'ai_waiting_input' || state === 'error' || state === 'stopped';
};

type ConversationListSyncSnapshot = {
  conversations: TChatConversation[];
  generatingConversationIds: Set<number>;
  completionUnreadConversationIds: Set<number>;
};

const listeners = new Set<() => void>();

let isStoreInitialized = false;
let conversationsState: TChatConversation[] = [];
let generatingConversationIdsState = new Set<number>();
let completionUnreadConversationIdsState = new Set<number>();
let conversation_idsState = new Set<number>();
let activeConversationIdState: number | null = null;
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
          const extra = conv.extra as
            | {
                is_health_check?: boolean;
                team_id?: string;
                teamId?: string;
                companionSession?: boolean;
                companionId?: string;
                channelPlatform?: string;
                orchestrator_run_id?: string;
                orchestrator_task_id?: string;
                multi_agent?: { enabled?: boolean };
              }
            | undefined;
          const isCompanionConversation =
            !!extra?.companionSession || !!extra?.companionId || !!extra?.channelPlatform;
          // 智能编排 (orchestrator) WORKER conversations are spawned by the Run
          // engine to execute fleet-member tasks; build_worker_extra stamps them
          // with BOTH `orchestrator_run_id` AND `orchestrator_task_id`. The LEAD
          // conversation (nomi_run_create write-back) carries only
          // `orchestrator_run_id` (no task_id) and must stay visible — it hosts
          // the DAG rail. So key the hide filter on the worker-only marker
          // `orchestrator_task_id`. Workers live under the orchestrator Run view,
          // never in this work conversation list — hide them like companion rows.
          const isOrchestratorWorkerConversation = !!extra?.orchestrator_task_id;
          // Multi-agent: the LEAD is the user's own conversation (it carries the
          // frontend-only `multi_agent` config the user toggled on); the backend
          // never writes `multi_agent` into a teammate's extra. So hide only
          // teammate conversations (teamId but no enabled multi_agent), keeping
          // the lead visible in the sidebar.
          const inTeam = !!extra?.team_id || !!extra?.teamId;
          const isMultiAgentLead = inTeam && extra?.multi_agent?.enabled === true;
          const hideAsTeammate = inTeam && !isMultiAgentLead;
          return (
            extra?.is_health_check !== true &&
            !hideAsTeammate &&
            !isCompanionConversation &&
            !isOrchestratorWorkerConversation
          );
        });
        conversationsState = filteredData;
        // Use ALL conversation IDs (including team/legacy health-check rows) so the
        // responseStream listener recognises them as known and doesn't
        // trigger an infinite refreshConversations loop.
        conversation_idsState = new Set(items.map((conversation) => conversation.id));
        emitStoreChange();
        return;
      }

      conversationsState = [];
      conversation_idsState = new Set();
      emitStoreChange();
    })
    .catch((error) => {
      console.error('[SessionList] Failed to load conversations:', error);
      conversationsState = [];
      conversation_idsState = new Set();
      emitStoreChange();
    });
};

const markGenerating = (conversation_id: number) => {
  if (generatingConversationIdsState.has(conversation_id)) {
    return;
  }

  generatingConversationIdsState = new Set(generatingConversationIdsState).add(conversation_id);
  emitStoreChange();
};

const clearGenerating = (conversation_id: number) => {
  if (!generatingConversationIdsState.has(conversation_id)) {
    return;
  }

  const next = new Set(generatingConversationIdsState);
  next.delete(conversation_id);
  generatingConversationIdsState = next;
  emitStoreChange();
};

const markCompletionUnread = (conversation_id: number) => {
  if (completionUnreadConversationIdsState.has(conversation_id)) {
    return;
  }

  completionUnreadConversationIdsState = new Set(completionUnreadConversationIdsState).add(conversation_id);
  emitStoreChange();
};

const clearCompletionUnreadState = (conversation_id: number) => {
  if (!completionUnreadConversationIdsState.has(conversation_id)) {
    return;
  }

  const next = new Set(completionUnreadConversationIdsState);
  next.delete(conversation_id);
  completionUnreadConversationIdsState = next;
  emitStoreChange();
};

const setActiveConversationState = (conversation_id: number | null) => {
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
      clearGenerating(event.conversation_id);
      clearCompletionUnreadState(event.conversation_id);
    }
    refreshConversations();
  });
  ipcBridge.conversation.responseStream.on((message) => {
    const conversation_id = message.conversation_id;
    if (!conversation_id) {
      return;
    }

    if (!conversation_idsState.has(conversation_id)) {
      refreshConversations();
    }

    if (isTerminalStreamMessage(message)) {
      const wasGenerating = generatingConversationIdsState.has(conversation_id);
      if (wasGenerating && activeConversationIdState !== conversation_id) {
        markCompletionUnread(conversation_id);
      }
      clearGenerating(conversation_id);
      return;
    }

    if (isGeneratingStreamMessage(message.type)) {
      markGenerating(conversation_id);
    }
  });
  ipcBridge.conversation.turnCompleted.on((event) => {
    if (isTerminalTurnState(event.state) && activeConversationIdState !== event.session_id) {
      markCompletionUnread(event.session_id);
    }
    clearGenerating(event.session_id);
    refreshConversations();
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

  const clearCompletionUnread = useCallback((conversation_id: number) => {
    clearCompletionUnreadState(conversation_id);
  }, []);

  const setActiveConversation = useCallback((conversation_id: number | null) => {
    setActiveConversationState(conversation_id);
  }, []);

  const isConversationGenerating = useCallback(
    (conversation_id: number) => {
      return generatingConversationIds.has(conversation_id);
    },
    [generatingConversationIds]
  );

  const hasCompletionUnread = useCallback(
    (conversation_id: number) => {
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
