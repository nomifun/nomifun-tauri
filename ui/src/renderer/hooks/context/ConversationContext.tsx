/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import type { ConversationId, CronJobId, MessageId } from '@/common/types/ids';

import type { IConversationMcpStatus } from '@/common/config/storage';
import React, { createContext, useContext } from 'react';

/**
 * Conversation context interface
 * 会话上下文接口
 */
export interface ConversationContextValue {
  /**
   * Conversation ID
   * 会话 ID
   */
  conversation_id: ConversationId;

  /**
   * Workspace directory path
   * 工作空间目录路径
   */
  workspace?: string;

  /**
   * Conversation type
   * 会话类型
   */
  type: 'acp' | 'openclaw-gateway' | 'nanobot' | 'remote' | 'nomi';

  /**
   * Cron job ID (if this conversation was created by a scheduled task)
   */
  cron_job_id?: CronJobId;

  /**
   * When true, platform chat components should hide the SendBox (for example, projected participant transcripts).
   */
  hideSendBox?: boolean;

  /**
   * Immutable transcript surface. Unlike `hideSendBox`, this is a capability
   * boundary: consumers must not send, confirm, warm up a runtime, or persist
   * conversation changes while it is set.
   */
  readOnly?: boolean;

  /**
   * True while the current conversation turn is still producing output.
   * Message rendering uses this to keep the tail in "正文 + 过程收据" mode
   * until the request actually settles.
   */
  isProcessing?: boolean;

  /** Authoritative root id of the active turn when the platform exposes it. */
  activeTurnId?: MessageId;

  /** User message that initiated `activeTurnId`, when this was a visible request. */
  activeRequestMessageId?: MessageId;

  /**
   * Loaded skill names for this conversation (snapshot from conversation.extra.skills).
   * Surfaced inside the SendBox `+` menu so users can review/jump to active skills.
   */
  loadedSkills?: string[];

  /**
   * Structured MCP status snapshot for this conversation (from
   * conversation.extra.mcp_statuses).
   */
  loadedMcpStatuses?: IConversationMcpStatus[];
}

/**
 * Conversation context
 * 会话上下文 - 提供会话级别的信息，如工作空间路径
 */
const ConversationContext = createContext<ConversationContextValue | null>(null);

/**
 * Conversation context provider
 * 会话上下文提供者
 */
export const ConversationProvider: React.FC<{
  children: React.ReactNode;
  value: ConversationContextValue;
}> = ({ children, value }) => {
  return <ConversationContext.Provider value={value}>{children}</ConversationContext.Provider>;
};

/**
 * Hook to safely use conversation context (returns null if not in provider)
 * 安全使用会话上下文的 Hook（如果不在 provider 中则返回 null）
 */
export const useConversationContextSafe = (): ConversationContextValue | null => {
  return useContext(ConversationContext);
};
