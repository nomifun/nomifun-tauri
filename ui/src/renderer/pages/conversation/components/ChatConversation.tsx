/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { ipcBridge } from '@/common';
import type { IConversationMcpStatus, IProvider, TChatConversation, TProviderWithModel } from '@/common/config/storage';
import addChatIcon from '@/renderer/assets/icons/add-chat.svg';
import { CronJobManager } from '@/renderer/pages/cron';
import { usePresetAssistantInfo, resolveAssistantConfigId } from '@/renderer/hooks/agent/usePresetAssistantInfo';
import { iconColors } from '@/renderer/styles/colors';
import { Button, Dropdown, Menu, Message, Tooltip, Typography } from '@arco-design/web-react';
import { History } from '@icon-park/react';
import React, { useCallback, useMemo, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import useSWR from 'swr';
import { emitter } from '../../../utils/emitter';
import AcpChat from '../platforms/acp/AcpChat';
import ChatLayout from './ChatLayout';
import ChatSlider from './ChatSlider.tsx';
import NanobotChat from '../platforms/nanobot/NanobotChat';
import OpenClawChat from '../platforms/openclaw/OpenClawChat';
import RemoteChat from '../platforms/remote/RemoteChat';
import { saveNomiDefaultModel } from '@/renderer/pages/guid/hooks/agentSelectionUtils';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { getConversationCreateErrorMessage } from '@/renderer/pages/conversation/utils/conversationCreateError';
import NomiChat from '../platforms/nomi/NomiChat';
import { useNomiModelSelection } from '../platforms/nomi/useNomiModelSelection';
import { OrchestrationProvider } from '../orchestration/OrchestrationContext';
import OrchestrationTopPanel from '../orchestration/OrchestrationTopPanel';
import ConversationContentSwitcher from '../orchestration/ConversationContentSwitcher';
import StarOfficeMonitorCard from '../platforms/openclaw/StarOfficeMonitorCard.tsx';
// import SkillRuleGenerator from './components/SkillRuleGenerator'; // Temporarily hidden

/** Check whether a specific skill is mounted on the conversation. */
const hasLoadedSkill = (conversation: TChatConversation | undefined, skillName: string): boolean => {
  const skills = (conversation?.extra as { skills?: string[] } | undefined)?.skills;
  return skills?.includes(skillName) ?? false;
};

const _AssociatedConversation: React.FC<{ conversation_id: number }> = ({ conversation_id }) => {
  const { data } = useSWR(['getAssociateConversation', conversation_id], () =>
    ipcBridge.conversation.getAssociateConversation.invoke({ conversation_id })
  );
  const navigate = useNavigate();
  const list = useMemo(() => {
    if (!data?.length) return [];
    return data.filter((conversation) => conversation.id !== conversation_id);
  }, [data]);
  if (!list.length) return null;
  return (
    <Dropdown
      droplist={
        <Menu
          onClickMenuItem={(key) => {
            Promise.resolve(navigate(`/conversation/${key}`)).catch((error) => {
              console.error('Navigation failed:', error);
            });
          }}
        >
          {list.map((conversation) => {
            return (
              <Menu.Item key={String(conversation.id)}>
                <Typography.Ellipsis className={'max-w-300px'}>{conversation.name}</Typography.Ellipsis>
              </Menu.Item>
            );
          })}
        </Menu>
      }
      trigger={['click']}
    >
      <Button
        size='mini'
        icon={
          <History
            theme='filled'
            size='14'
            fill={iconColors.primary}
            strokeWidth={2}
            strokeLinejoin='miter'
            strokeLinecap='square'
          />
        }
      ></Button>
    </Dropdown>
  );
};

const _AddNewConversation: React.FC<{ conversation: TChatConversation }> = ({ conversation }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const isCreatingRef = useRef(false);
  if (!conversation.extra?.workspace) return null;
  return (
    <Tooltip content={t('conversation.workspace.createNewConversation')}>
      <Button
        size='mini'
        icon={<img src={addChatIcon} alt='Add chat' className='w-14px h-14px block m-auto' />}
        onClick={async () => {
          if (isCreatingRef.current) return;
          isCreatingRef.current = true;
          try {
            // Fetch latest conversation from DB to ensure session_mode is current
            const latest = await getConversationOrNull(conversation.id);
            const source = latest || conversation;
            // Conversations now use INTEGER AUTOINCREMENT primary keys minted by
            // the backend (numeric-id spec §5). We must NOT mint the id on the
            // frontend; strip the source id and let the clone endpoint assign a
            // fresh one, then route to the real id the backend returns. This is
            // the same server-mints-the-id pattern as `msg_id`.
            const { id: _sourceId, ...sourceWithoutId } = source;
            const created = await ipcBridge.conversation.createWithConversation.invoke({
              conversation: {
                ...sourceWithoutId,
                created_at: Date.now(),
                modified_at: Date.now(),
                // Clear ACP session fields to prevent new conversation from inheriting old session context
                extra:
                  source.type === 'acp'
                    ? { ...source.extra, acp_session_id: undefined, acp_session_updated_at: undefined }
                    : source.extra,
              } as TChatConversation,
            });
            void navigate(`/conversation/${created.id}`);
            emitter.emit('chat.history.refresh');
          } catch (error) {
            console.error('Failed to create conversation:', error);
            Message.error(getConversationCreateErrorMessage(error, t));
          } finally {
            isCreatingRef.current = false;
          }
        }}
      />
    </Tooltip>
  );
};

type NomiConversation = Extract<TChatConversation, { type: 'nomi' }>;

const NomiConversationPanel: React.FC<{ conversation: NomiConversation; sliderTitle: React.ReactNode }> = ({
  conversation,
  sliderTitle,
}) => {
  const onSelectModel = useCallback(
    async (_provider: IProvider, modelName: string) => {
      const selected = { ..._provider, use_model: modelName } as TProviderWithModel;
      // Kill running agent on model switch — will be rebuilt with new model on next message
      await ipcBridge.conversation.stop.invoke({ conversation_id: conversation.id });
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) void saveNomiDefaultModel(_provider.id, modelName);
      return Boolean(ok);
    },
    [conversation.id]
  );

  const modelSelection = useNomiModelSelection({
    initialModel: conversation.model,
    onSelectModel,
  });
  const workspaceEnabled = Boolean(conversation.extra?.workspace);
  const { info: presetAssistantInfo } = usePresetAssistantInfo(conversation);
  const nomiAssistantId = resolveAssistantConfigId(conversation) ?? undefined;

  const chatLayoutProps = {
    title: conversation.name,
    siderTitle: sliderTitle,
    sider: <ChatSlider conversation={conversation} />,
    headerExtra: (
      <div className='flex items-center gap-8px'>
        {/* 编排画布 (Option B): the orchestration canvas + run controls live in a
            collapsible panel pinned to the TOP of the content area (no floating
            overlay, no right-rail tab). The header keeps just the existing
            capability controls (CronJobManager). */}
        <CronJobManager
          conversation_id={conversation.id}
          cron_job_id={conversation.extra?.cron_job_id as string | undefined}
          hasCronSkill={hasLoadedSkill(conversation, 'cron')}
        />
      </div>
    ),
    workspaceEnabled,
    workspacePath: conversation.extra?.workspace,
    isTemporaryWorkspace: (conversation.extra as { is_temporary_workspace?: boolean } | undefined)
      ?.is_temporary_workspace,
    backend: 'nomi' as const,
    presetAssistant: presetAssistantInfo ? { ...presetAssistantInfo, id: nomiAssistantId } : undefined,
  };

  return (
    <OrchestrationProvider conversation={conversation}>
      <ChatLayout {...chatLayoutProps} conversation_id={conversation.id}>
        {/* 编排画布:左右分屏 —— 主 agent 聊天在左(flex-1),编排画布作为右侧
            可拖拽改宽 / 可收起的侧栏。OrchestrationTopPanel 在 run 不存在时渲染
            null,普通会话看起来与从前一致。点右侧画布节点把 worker 转录投射进
            左侧聊天区(默认 main)。 */}
        <div className='flex flex-row flex-1 min-h-0'>
          <div className='flex-1 min-h-0 flex flex-col'>
            {/* Content-area projection (会话原生编排, F7): keeps NomiChat ALWAYS
                mounted and just toggles its visibility, overlaying a clicked DAG
                worker node's read-only transcript when a node is projected. Node
                clicks in the right canvas pane project the worker transcript into
                this chat region; default main. */}
            <ConversationContentSwitcher>
              <NomiChat
                conversation_id={conversation.id}
                workspace={conversation.extra.workspace}
                modelSelection={modelSelection}
                session_mode={conversation.extra?.session_mode}
                cron_job_id={(conversation.extra as { cron_job_id?: string })?.cron_job_id}
                loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
                loadedMcpServers={(conversation.extra as { mcp_servers?: string[] } | undefined)?.mcp_servers}
                loadedMcpStatuses={
                  (conversation.extra as { mcp_statuses?: IConversationMcpStatus[] } | undefined)?.mcp_statuses
                }
                agent_name={presetAssistantInfo?.name}
              />
            </ConversationContentSwitcher>
          </div>
          <OrchestrationTopPanel />
        </div>
      </ChatLayout>
    </OrchestrationProvider>
  );
};

const ChatConversation: React.FC<{
  conversation?: TChatConversation;
  hideSendBox?: boolean;
}> = ({ conversation, hideSendBox }) => {
  const { t } = useTranslation();
  const workspaceEnabled = Boolean(conversation?.extra?.workspace);

  const isNomiConversation = conversation?.type === 'nomi';

  // 使用统一的 Hook 获取预设助手信息（ACP/Codex 会话）
  // Use unified hook for preset assistant info (ACP/Codex conversations)
  const acpConversation = isNomiConversation ? undefined : conversation;
  const { info: presetAssistantInfo, isLoading: isLoadingPreset } = usePresetAssistantInfo(acpConversation);
  const acpAssistantId = acpConversation ? (resolveAssistantConfigId(acpConversation) ?? undefined) : undefined;

  const conversationAgentName = (conversation?.extra as { agent_name?: string } | undefined)?.agent_name;
  const assistantDisplayName = presetAssistantInfo?.name || conversationAgentName;

  const conversationNode = useMemo(() => {
    if (!conversation || isNomiConversation) return null;
    switch (conversation.type) {
      case 'acp':
        {
          const extra = conversation.extra as { backend?: string; current_model_id?: string };
          return (
            <AcpChat
              key={conversation.id}
              conversation_id={conversation.id}
              workspace={conversation.extra?.workspace}
              backend={extra.backend || 'claude'}
              initialModelId={extra.current_model_id}
              session_mode={conversation.extra?.session_mode}
              agent_name={assistantDisplayName}
              cron_job_id={(conversation.extra as { cron_job_id?: string })?.cron_job_id}
              hideSendBox={hideSendBox}
              loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
              loadedMcpServers={(conversation.extra as { mcp_servers?: string[] } | undefined)?.mcp_servers}
              loadedMcpStatuses={
                (conversation.extra as { mcp_statuses?: IConversationMcpStatus[] } | undefined)?.mcp_statuses
              }
            ></AcpChat>
          );
        }
      case 'gemini':
        // Legacy Gemini conversation: the dedicated Gemini runtime has been
        // removed. The message history is still served by the shared messages
        // table, so AcpChat renders it fine. The composer is left enabled —
        // any send attempt will get a BadRequest from the factory branch in
        // nomifun-common/src/enums.rs → factory.rs, surfacing a clear error
        // to the user.
        return (
          <AcpChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace}
            backend='gemini'
            initialModelId={(conversation.extra as { current_model_id?: string } | undefined)?.current_model_id}
            agent_name={assistantDisplayName}
            cron_job_id={(conversation.extra as { cron_job_id?: string })?.cron_job_id}
            hideSendBox={hideSendBox}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
            loadedMcpServers={(conversation.extra as { mcp_servers?: string[] } | undefined)?.mcp_servers}
            loadedMcpStatuses={
              (conversation.extra as { mcp_statuses?: IConversationMcpStatus[] } | undefined)?.mcp_statuses
            }
          />
        );
      case 'codex': // Legacy: codex now uses ACP protocol
        return (
          <AcpChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace}
            backend='codex'
            initialModelId={(conversation.extra as { current_model_id?: string } | undefined)?.current_model_id}
            agent_name={assistantDisplayName}
            hideSendBox={hideSendBox}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
            loadedMcpServers={(conversation.extra as { mcp_servers?: string[] } | undefined)?.mcp_servers}
            loadedMcpStatuses={
              (conversation.extra as { mcp_statuses?: IConversationMcpStatus[] } | undefined)?.mcp_statuses
            }
          />
        );
      case 'openclaw-gateway':
        return (
          <OpenClawChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            cron_job_id={(conversation.extra as { cron_job_id?: string })?.cron_job_id}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
          />
        );
      case 'nanobot':
        return (
          <NanobotChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            cron_job_id={(conversation.extra as { cron_job_id?: string })?.cron_job_id}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
          />
        );
      case 'remote':
        return (
          <RemoteChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            cron_job_id={(conversation.extra as { cron_job_id?: string })?.cron_job_id}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
          />
        );
      default:
        return null;
    }
  }, [conversation, isNomiConversation, assistantDisplayName, hideSendBox]);

  const sliderTitle = useMemo(() => {
    return (
      <div className='flex items-center justify-between'>
        <span className='text-16px font-bold text-t-primary'>{t('conversation.workspace.title')}</span>
      </div>
    );
  }, [t]);

  if (conversation && conversation.type === 'nomi') {
    return <NomiConversationPanel key={conversation.id} conversation={conversation} sliderTitle={sliderTitle} />;
  }

  // 如果有预设助手信息，使用预设助手的 logo 和名称；加载中时不进入 fallback；否则使用 backend 的 logo
  // If preset assistant info exists, use preset logo/name; while loading, avoid fallback; otherwise use backend logo
  const chatLayoutProps = presetAssistantInfo
    ? {
        presetAssistant: { ...presetAssistantInfo, id: acpAssistantId },
      }
    : isLoadingPreset
      ? {} // Still loading custom agents — avoid showing backend logo prematurely
      : {
          backend:
            conversation?.type === 'acp'
              ? conversation?.extra?.backend
              : // `nomi` conversations are handled by the early return above and can
                // never reach this branch, so the chain starts at `codex`.
                conversation?.type === 'codex'
                ? 'codex'
                : conversation?.type === 'openclaw-gateway'
                  ? 'openclaw-gateway'
                  : conversation?.type === 'nanobot'
                    ? 'nanobot'
                    : conversation?.type === 'remote'
                      ? 'remote'
                      : undefined,
          agent_name: conversationAgentName,
        };

  const headerExtraNode = (
    <div className='flex items-center gap-8px'>
      {conversation?.type === 'openclaw-gateway' && (
        <div className='shrink-0'>
          <StarOfficeMonitorCard conversation_id={conversation.id} />
        </div>
      )}
      {conversation && (
        <div className='shrink-0'>
          <CronJobManager
            conversation_id={conversation.id}
            cron_job_id={conversation.extra?.cron_job_id as string | undefined}
            hasCronSkill={hasLoadedSkill(conversation, 'cron')}
          />
        </div>
      )}
    </div>
  );

  return (
    <ChatLayout
      title={conversation?.name}
      {...chatLayoutProps}
      headerExtra={headerExtraNode}
      siderTitle={sliderTitle}
      sider={<ChatSlider conversation={conversation} />}
      workspaceEnabled={workspaceEnabled}
      workspacePath={conversation?.extra?.workspace}
      isTemporaryWorkspace={
        (conversation?.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
      }
      conversation_id={conversation?.id}
    >
      {conversationNode}
    </ChatLayout>
  );
};

export default ChatConversation;
