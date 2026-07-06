/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { IConversationMcpStatus, IProvider, TChatConversation, TProviderWithModel } from '@/common/config/storage';
import addChatIcon from '@/renderer/assets/icons/add-chat.svg';
import { CronJobManager } from '@/renderer/pages/cron';
import { usePresetAssistantInfo, resolveAssistantConfigId } from '@/renderer/hooks/agent/usePresetAssistantInfo';
import { iconColors } from '@/renderer/styles/colors';
import { Button, Dropdown, Menu, Message, Tooltip, Typography } from '@arco-design/web-react';
import { History } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
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
import { configService } from '@/common/config/configService';
import { useModelProviderList } from '@/renderer/hooks/agent/useModelProviderList';
import { resolveHealModel } from '../platforms/nomi/healConversationModel';
import { getConversationOrNull, seedConversationCache } from '@/renderer/pages/conversation/utils/conversationCache';
import { getConversationCreateErrorMessage } from '@/renderer/pages/conversation/utils/conversationCreateError';
import { isConversationProcessing } from '@/renderer/pages/conversation/utils/conversationRuntime';
import NomiChat from '../platforms/nomi/NomiChat';
import { useNomiModelSelection } from '../platforms/nomi/useNomiModelSelection';
import CompanionChatPanel from '@/renderer/pages/nomi/companion/CompanionChatPanel';
import GuidCollaboratorSelector from '@/renderer/pages/guid/components/GuidCollaboratorSelector';
import ClusterModePill from './ClusterModePill';
import type { TModelRange, TModelRef } from '@/common/types/orchestrator/orchestratorTypes';
import { OrchestrationProvider } from '../orchestration/OrchestrationContext';
import OrchestrationTopPanel from '../orchestration/OrchestrationTopPanel';
import ConversationContentSwitcher from '../orchestration/ConversationContentSwitcher';
import PlanApprovalBanner from '../orchestration/PlanApprovalBanner';
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
            seedConversationCache(created);
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
  // 协作模型池:每会话的真源是 `extra.orchestrator_model_range`。水合时 models[0] 是
  // 主模型(lead/planner),其余为协作池。面板以 `key={conversation.id}` 挂载,切换会话
  // 会重挂并按新会话 extra 重新水合。
  const [collaborators, setCollaboratorsState] = useState<TModelRef[]>(() => {
    const range = conversation.extra?.orchestrator_model_range;
    return range?.mode === 'range' ? range.models.slice(1) : [];
  });

  // 写回:主模型 FIRST(models[0]=lead/planner)+ 协作池,按 `${provider_id} ${model}`
  // 去重(与旧 useGuidSend 一致),整体作为 range 落到会话 extra;后端 caps_orchestrator
  // 的 read_conversation_model_range 只读回来构建 run 的 fleet(确定性,不经 LLM)。
  const persistModelRange = useCallback(
    async (mainRef: TModelRef | null, collabs: TModelRef[]) => {
      if (!mainRef) return;
      const seen = new Set<string>();
      const models = [mainRef, ...collabs].filter((r) => {
        if (!r?.provider_id || !r.model) return false;
        const key = `${r.provider_id} ${r.model}`;
        if (seen.has(key)) return false;
        seen.add(key);
        return true;
      });
      const orchestrator_model_range: TModelRange = { mode: 'range', models };
      // extra 顶层浅合并(后端保留同级 extra 键),只覆盖 orchestrator_model_range。
      // 内部吞掉失败并 console.error:未持久化的模型范围是低危状态(下次选择即重试),
      // 不值得打断用户;此处兜底也让两处 `void persistModelRange(...)` 调用点不会产生
      // 未处理的 promise rejection。
      try {
        await ipcBridge.conversation.update.invoke({
          id: conversation.id,
          updates: { extra: { orchestrator_model_range } as TChatConversation['extra'] },
        });
      } catch (err) {
        console.error('[ChatConversation] persist orchestrator_model_range failed', err);
      }
    },
    [conversation.id]
  );

  const { t } = useTranslation();
  const onSelectModel = useCallback(
    async (_provider: IProvider, modelName: string) => {
      const selected = { ..._provider, use_model: modelName } as TProviderWithModel;
      // Kill running agent on model switch — will be rebuilt with new model on next message
      await ipcBridge.conversation.stop.invoke({ conversation_id: conversation.id });
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) {
        void saveNomiDefaultModel(_provider.id, modelName);
        // 主模型即 range 的 models[0](lead/planner):切换主模型后同步重写 range,
        // 让协作池仍钉在新主模型之后。
        void persistModelRange({ provider_id: _provider.id, model: modelName }, collaborators);
      }
      return Boolean(ok);
    },
    [conversation.id, persistModelRange, collaborators]
  );

  const modelSelection = useNomiModelSelection({
    initialModel: conversation.model,
    onSelectModel,
  });

  // 主模型引用(range 的 models[0]),供协作选择器钉选与写回。
  const mainModelRef = useMemo<TModelRef | null>(
    () =>
      modelSelection.current_model
        ? { provider_id: modelSelection.current_model.id, model: modelSelection.current_model.use_model }
        : null,
    [modelSelection.current_model?.id, modelSelection.current_model?.use_model]
  );

  const onCollaboratorsChange = useCallback(
    (next: TModelRef[]) => {
      setCollaboratorsState(next);
      void persistModelRange(mainModelRef, next);
    },
    [mainModelRef, persistModelRange]
  );

  // 会话内「协作模型」选择器:紧跟主模型选择器渲染。集群开关另放到权限旁边，
  // 避免把主模型 / 协作模型的关系打断。
  const collaboratorSelectorNode = (
    <GuidCollaboratorSelector
      value={collaborators}
      onChange={onCollaboratorsChange}
      mainModel={mainModelRef}
      className='nomi-sendbox-model-btn'
    />
  );

  const { providers: healProviders, getAvailableModels: healGetAvailable } = useModelProviderList();
  useEffect(() => {
    if (!healProviders.length) return;
    const saved = configService.get('nomi.defaultModel');
    const heal = resolveHealModel(
      conversation.model,
      healProviders,
      healGetAvailable,
      saved && typeof saved === 'object' && 'id' in saved ? saved : undefined
    );
    if (!heal) return;
    void (async () => {
      const selected = { ...heal.provider, use_model: heal.use_model } as TProviderWithModel;
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) {
        void saveNomiDefaultModel(heal.provider.id, heal.use_model);
        Message.info(t('conversation.chat.modelHealedToDefault', { model: heal.use_model }));
      }
    })();
    // 仅在会话或供应商列表变化时评估
  }, [conversation.id, conversation.model?.id, conversation.model?.use_model, healProviders, healGetAvailable, t]);

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
          <div className='flex-1 min-w-0 min-h-0 flex flex-col'>
            {/* 智能编排「编排后不自动执行」提示条:仅当本会话关联的 run 停在
                awaiting_plan_approval 时显示,复用批准 IPC;其余情况渲染 null。 */}
            <PlanApprovalBanner />
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
                collaboratorSelectorNode={collaboratorSelectorNode}
                extraRightTools={<ClusterModePill conversation={conversation} />}
                isProcessing={isConversationProcessing(conversation)}
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
    // 桌面伙伴的专属会话（单会话契约）走受限面板：保留锁定模型/隐藏高级控制/强制 yolo/
    // 固定工作区（详见 CompanionChatPanel → CompanionConversation），而非全功能编排面板。
    if (conversation.extra?.companionSession) {
      return <CompanionChatPanel key={conversation.id} conversation={conversation} />;
    }
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
