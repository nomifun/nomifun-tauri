/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Spin } from '@arco-design/web-react';
import type { TChatConversation } from '@/common/config/storage';
import ChatSlider from '@/renderer/pages/conversation/components/ChatSlider';
import ExecutionConversationLayout from '@/renderer/pages/conversation/execution/ExecutionConversationLayout';
import { useCompanion } from '../useNomi';
import CompanionConversation from './CompanionConversation';
import CompanionModelControl from '../CompanionModelControl';
import type { WorkspaceExtraTab } from '@/renderer/pages/conversation/Workspace/types';

type NomiConversation = Extract<TChatConversation, { type: 'nomi' }>;

interface Props {
  /** A desktop-companion's single per-companion nomi session (extra.companion_session). */
  conversation: NomiConversation;
  /** Conversation-owned resources exposed in the shared right-hand tool rail. */
  extraTabs?: WorkspaceExtraTab[];
}

/**
 * 在「会话」视图里承载桌面伙伴聊天的入口面板。
 *
 * 取代旧的 /nomi 配置中心「聊天」Tab（ChatTab）：迁移后伙伴聊天统一从会话列表的
 * 「桌面伙伴」分组进入标准 `/conversation/:id`。ChatConversation 见到
 * `type==='nomi' && extra.companion_session` 即渲染本面板（而非全功能 NomiConversationPanel），
 * 从而保留伙伴专属约束（锁定模型 / 隐藏高级控制 / 强制 yolo / 固定工作区，详见
 * CompanionConversation）。
 *
 * 与 ChatTab 的差别：会话对象已由会话页 SWR 载入并传入，故无需再 ensureCompanionSession /
 * 二次载入——本面板只负责：① 由 `extra.companion_id` 解析伙伴 profile（模型唯一事实源
 * + 乐观 patch 通道）；② 模型未配置态的引导（含模型配置入口）；③ 交给
 * CompanionConversation 渲染受限会话主体。
 */
const CompanionChatPanel: React.FC<Props> = ({ conversation, extraTabs }) => {
  const { t } = useTranslation();
  const companionId = conversation.extra?.companion_id ?? null;
  const companion = useCompanion(companionId);
  const { profile, status } = companion;
  const workspace = conversation.extra?.workspace ?? '';

  const renderInExecutionShell = (content: React.ReactNode, showModelControl = false) => (
    <ExecutionConversationLayout
      title={conversation.name}
      conversation_id={conversation.id}
      hideAdvancedControls
      disableRename
      workspaceEnabled={Boolean(workspace)}
      workspacePath={workspace || undefined}
      sider={<ChatSlider conversation={conversation} extraTabs={extraTabs} />}
      siderTitle={<span className='text-16px font-bold text-t-primary'>{t('conversation.workspace.title')}</span>}
      workspaceExtraTabs={extraTabs}
      headerExtra={showModelControl ? <CompanionModelControl companion={companion} /> : undefined}
    >
      {content}
    </ExecutionConversationLayout>
  );

  // 会话被标记为伙伴会话但缺 companionId（异常数据）：兜底，避免空白面板。
  if (!companionId) {
    return renderInExecutionShell(
      <div className='flex-1 flex items-center justify-center text-13px text-t-tertiary px-16px text-center'>
        {t('nomi.companion.chatError')}
      </div>,
    );
  }

  // 解析伙伴 profile 中（切伙伴时 useCompanion 同步置空，避免 stale）。
  if (!profile) {
    return renderInExecutionShell(
      <div className='flex-1 flex justify-center items-center py-40px'>
        <Spin />
      </div>,
    );
  }

  // 模型未配置：把模型配置入口（唯一事实源）放在引导态，配置后伙伴会话即可对话。
  const modelConfigured = status
    ? status.model_configured
    : profile.model !== null;
  if (!modelConfigured) {
    return renderInExecutionShell(
      <div className='flex flex-col h-full min-h-0 items-center justify-center gap-14px px-16px text-center'>
        <CompanionModelControl companion={companion} />
        <div className='text-13px text-t-tertiary'>{t('nomi.chat.modelMissing')}</div>
      </div>,
    );
  }

  return renderInExecutionShell(
    <CompanionConversation conversation={conversation} companion={companion} />,
    true,
  );
};

export default CompanionChatPanel;
