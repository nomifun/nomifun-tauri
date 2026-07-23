/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ConversationId } from '@/common/types/ids';
import { ipcBridge } from '@/common';
import type { IConversationMcpStatus, IProvider, TChatConversation, TProviderWithModel } from '@/common/config/storage';
import addChatIcon from '@/renderer/assets/icons/add-chat.svg';
import { CronJobManager } from '@/renderer/pages/cron';
import { usePresetInfo } from '@/renderer/hooks/agent/usePresetInfo';
import { iconColors } from '@/renderer/styles/colors';
import { Button, Dropdown, Menu, Message, Tooltip, Typography } from '@arco-design/web-react';
import { ChartHistogram, History, Terminal } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import useSWR from 'swr';
import { emitter } from '../../../utils/emitter';
import AcpChat from '../platforms/acp/AcpChat';
import ChatLayout, { type ChatLayoutProps } from './ChatLayout';
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
import {
  toAppliedCollaborationTemplate,
  type AppliedCollaborationTemplate,
} from '@/renderer/components/collaboration/collaborationTemplateModel';
import CollaborationPolicyControl, {
  type CollaborationPolicyValue,
} from '@/renderer/components/collaboration/CollaborationPolicyControl';
import type { TExecutionModelPool, TExecutionModelRef } from '@/common/types/agentExecution/agentExecutionTypes';
import { ExecutionProvider } from '../execution/ExecutionContext';
import ExecutionConversationLayout from '../execution/ExecutionConversationLayout';
import ReadOnlyConversationView from '../execution/ReadOnlyConversationView';
import StarOfficeMonitorCard from '../platforms/openclaw/StarOfficeMonitorCard.tsx';
import NomiSessionMetricsPanel from '../platforms/nomi/NomiSessionMetricsPanel';
import ConversationTerminalPanel from './ConversationTerminalPanel';
import { useExecutionModelPool } from '../execution/useExecutionModelPool';
import { reconcileModelRefs, sameModelRefs } from '../execution/executionModelRefs';
// import SkillRuleGenerator from './components/SkillRuleGenerator'; // Temporarily hidden

/** Check whether a specific skill is mounted on the conversation. */
const hasLoadedSkill = (conversation: TChatConversation | undefined, skillName: string): boolean => {
  const skills = (conversation?.extra as { skills?: string[] } | undefined)?.skills;
  return skills?.includes(skillName) ?? false;
};

const buildConversationModelPool = (
  mainRef: TExecutionModelRef | null,
  collaborators: TExecutionModelRef[],
): TExecutionModelPool | null => {
  if (!mainRef?.provider_id || !mainRef.model) return null;
  const seen = new Set<string>();
  const models = [mainRef, ...collaborators].filter((candidate) => {
    if (!candidate.provider_id || !candidate.model) return false;
    const key = `${candidate.provider_id}\u0000${candidate.model}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  return models.length === 1 ? { mode: 'single', model: models[0] } : { mode: 'range', models };
};

const _AssociatedConversation: React.FC<{ conversation_id: ConversationId }> = ({ conversation_id }) => {
  const { data } = useSWR(['getAssociateConversation', conversation_id], () =>
    ipcBridge.conversation.getAssociateConversation.invoke({ conversation_id }),
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
              <Menu.Item key={conversation.id}>
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
            // Strip the source entity ID and let the clone endpoint mint a
            // fresh canonical ID, then route to the value returned by the backend.
            const { id: _sourceId, ...sourceWithoutId } = source;
            const {
              workspace: _sourceWorkspace,
              custom_workspace: _sourceCustomWorkspace,
              is_temporary_workspace: _sourceTemporaryWorkspace,
              temp_workspace_id: _sourceTempWorkspaceId,
              acp_session_id: _sourceAcpSessionId,
              acp_session_conversation_id: _sourceAcpSessionConversationId,
              acp_session_updated_at: _sourceAcpSessionUpdatedAt,
              current_mode_id: _sourceCurrentModeId,
              current_model_id: _sourceCurrentModelId,
              cached_config_options: _sourceCachedConfigOptions,
              pending_config_options: _sourcePendingConfigOptions,
              runtimeValidation: _sourceRuntimeValidation,
              sessionKey: _sourceSessionKey,
              ...freshExtra
            } = source.extra as TChatConversation['extra'] & Record<string, unknown>;
            const created = await ipcBridge.conversation.createWithConversation.invoke({
              conversation: {
                ...sourceWithoutId,
                created_at: Date.now(),
                modified_at: Date.now(),
                // This button creates an isolated conversation. Sharing the
                // current workspace/session must be an explicit operation.
                // The backend repeats this scrub as the authoritative boundary.
                extra: freshExtra,
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

const NomiConversationLayout: React.FC<{
  conversation: NomiConversation;
  chatLayoutProps: Omit<ChatLayoutProps, 'children' | 'workspaceCollaboration' | 'workspaceExtraTabs'>;
  modelSelection: React.ComponentProps<typeof NomiChat>['modelSelection'];
  collaboratorSelectorNode: React.ReactNode;
  collaborationPolicyNode: React.ReactNode;
  presetPresetName?: string;
}> = ({
  conversation,
  chatLayoutProps,
  modelSelection,
  collaboratorSelectorNode,
  collaborationPolicyNode,
  presetPresetName,
}) => {
  const { t } = useTranslation();
  const workspaceExtraTabs = useMemo(
    () => [
      {
        key: 'conversation-terminals',
        title: t('terminal.conversationPanel.tab'),
        icon: <Terminal size={18} />,
        content: <ConversationTerminalPanel conversationId={conversation.id} />,
      },
      {
        key: 'nomi-session-metrics',
        title: t('conversation.sessionMetrics.tab'),
        icon: <ChartHistogram size={18} />,
        content: <NomiSessionMetricsPanel conversation={conversation} />,
      },
    ],
    [conversation, t],
  );

  return (
    <ExecutionConversationLayout
      {...chatLayoutProps}
      sider={<ChatSlider conversation={conversation} extraTabs={workspaceExtraTabs} />}
      conversation_id={conversation.id}
      workspaceExtraTabs={workspaceExtraTabs}
    >
      <NomiChat
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        modelSelection={modelSelection}
        session_mode={conversation.extra?.session_mode}
        cron_job_id={conversation.cron_job_id}
        loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
        loadedMcpStatuses={
          (conversation.extra as { mcp_statuses?: IConversationMcpStatus[] } | undefined)?.mcp_statuses
        }
        agent_name={presetPresetName}
        collaboratorSelectorNode={collaboratorSelectorNode}
        extraRightTools={collaborationPolicyNode}
        isProcessing={isConversationProcessing(conversation)}
      />
    </ExecutionConversationLayout>
  );
};

const NomiConversationPanel: React.FC<{
  conversation: NomiConversation;
  sliderTitle: React.ReactNode;
}> = ({ conversation, sliderTitle }) => {
  const [collaborators, setCollaboratorsState] = useState<TExecutionModelRef[]>(() => {
    const pool = conversation.execution_model_pool;
    return pool?.mode === 'range' ? pool.models.slice(1) : [];
  });
  const [collaborationPolicy, setCollaborationPolicy] = useState<CollaborationPolicyValue>({
    delegationPolicy: conversation.delegation_policy ?? 'automatic',
    decisionPolicy: conversation.decision_policy ?? 'automatic',
  });
  const [selectedCollaborationTemplate, setSelectedCollaborationTemplate] =
    useState<AppliedCollaborationTemplate | null>(null);
  useEffect(() => {
    setCollaborationPolicy({
      delegationPolicy: conversation.delegation_policy ?? 'automatic',
      decisionPolicy: conversation.decision_policy ?? 'automatic',
    });
  }, [conversation.decision_policy, conversation.delegation_policy]);

  const storedExecutionTemplateId = conversation.execution_template_id ?? null;
  useEffect(() => {
    if (!storedExecutionTemplateId) {
      setSelectedCollaborationTemplate(null);
      return;
    }
    let cancelled = false;
    void ipcBridge.agentExecutionTemplate.get
      .invoke({ execution_template_id: storedExecutionTemplateId })
      .then((template) => {
        if (!cancelled) {
          setSelectedCollaborationTemplate(toAppliedCollaborationTemplate(template));
        }
      })
      .catch((error) => {
        console.error('[ChatConversation] Failed to resolve collaboration template:', error);
        if (!cancelled) setSelectedCollaborationTemplate(null);
      });
    return () => {
      cancelled = true;
    };
  }, [storedExecutionTemplateId]);
  const { configuredPairs, allPairs, isLoading: isModelCatalogLoading } = useExecutionModelPool();
  const collaboratorReconciliation = useMemo(
    () => (isModelCatalogLoading ? null : reconcileModelRefs(collaborators, configuredPairs, allPairs)),
    [allPairs, collaborators, configuredPairs, isModelCatalogLoading],
  );
  const activeCollaborators = collaboratorReconciliation?.active ?? [];

  const persistModelPool = useCallback(
    async (mainRef: TExecutionModelRef | null, collabs: TExecutionModelRef[]) => {
      const execution_model_pool = buildConversationModelPool(mainRef, collabs);
      if (!execution_model_pool) return;
      try {
        await ipcBridge.conversation.update.invoke({
          conversation_id: conversation.id,
          updates: { execution_model_pool },
        });
      } catch (err) {
        console.error('[ChatConversation] Failed to persist execution model pool:', err);
      }
    },
    [conversation.id],
  );

  const { t } = useTranslation();
  const onSelectModel = useCallback(
    async (_provider: IProvider, modelName: string) => {
      const selected = {
        ..._provider,
        use_model: modelName,
      } as TProviderWithModel;
      // Kill the running agent on model switch; it will be rebuilt with the
      // new model on the next message.
      await ipcBridge.conversation.stop.invoke({
        conversation_id: conversation.id,
      });
      const execution_model_pool = buildConversationModelPool(
        { provider_id: _provider.id, model: modelName },
        activeCollaborators,
      );
      if (!execution_model_pool) return false;
      const ok = await ipcBridge.conversation.update.invoke({
        conversation_id: conversation.id,
        // The lead model and its collaboration authority are one atomic
        // Conversation preference update; never expose a mixed intermediate
        // state to Gateway delegation.
        updates: { model: selected, execution_model_pool, execution_template_id: null },
      });
      if (ok) {
        setSelectedCollaborationTemplate(null);
        void saveNomiDefaultModel(_provider.id, modelName);
      }
      return Boolean(ok);
    },
    [activeCollaborators, conversation.id],
  );

  const modelSelection = useNomiModelSelection({
    initialModel: conversation.model,
    onSelectModel,
  });

  // Main model reference used by the collaboration selector.
  const mainModelRef = useMemo<TExecutionModelRef | null>(
    () =>
      modelSelection.current_model
        ? {
            provider_id: modelSelection.current_model.id,
            model: modelSelection.current_model.use_model,
          }
        : null,
    [modelSelection.current_model?.id, modelSelection.current_model?.use_model],
  );

  const onCollaboratorsChange = useCallback(
    (next: TExecutionModelRef[]) => {
      setCollaboratorsState(next);
      void persistModelPool(mainModelRef, next);
    },
    [mainModelRef, persistModelPool],
  );

  const persistCollaborationTemplate = useCallback(
    async (next: AppliedCollaborationTemplate | null) => {
      const previous = selectedCollaborationTemplate;
      setSelectedCollaborationTemplate(next);
      try {
        await ipcBridge.conversation.update.invoke({
          conversation_id: conversation.id,
          updates: {
            execution_template_id: next?.execution_template_id ?? null,
          },
        });
      } catch (error) {
        setSelectedCollaborationTemplate(previous);
        console.error('[ChatConversation] Failed to persist collaboration template:', error);
        Message.error(t('common.failed', { defaultValue: '保存协作方案失败' }));
      }
    },
    [conversation.id, selectedCollaborationTemplate, t],
  );

  useEffect(() => {
    if (!collaboratorReconciliation || collaboratorReconciliation.removed.length === 0) return;
    if (sameModelRefs(collaborators, collaboratorReconciliation.retained)) return;
    setCollaboratorsState(collaboratorReconciliation.retained);
    void persistModelPool(mainModelRef, collaboratorReconciliation.retained);
  }, [collaboratorReconciliation, collaborators, mainModelRef, persistModelPool]);

  // Collaboration selector stays adjacent to the main model selector.
  const collaboratorSelectorNode = (
    <GuidCollaboratorSelector
      value={activeCollaborators}
      onChange={onCollaboratorsChange}
      mainModel={mainModelRef}
      selectedTemplate={selectedCollaborationTemplate}
      workDir={conversation.extra?.workspace}
      onTemplateApply={(template) => void persistCollaborationTemplate(template)}
      onTemplateClear={() => void persistCollaborationTemplate(null)}
      className='nomi-sendbox-model-btn'
    />
  );

  const onCollaborationPolicyChange = useCallback(
    async (next: CollaborationPolicyValue) => {
      setCollaborationPolicy(next);
      try {
        await ipcBridge.conversation.update.invoke({
          conversation_id: conversation.id,
          updates: {
            delegation_policy: next.delegationPolicy,
            decision_policy: next.decisionPolicy,
          },
        });
      } catch (error) {
        console.error('[ChatConversation] Failed to persist collaboration policy:', error);
      }
    },
    [conversation.id],
  );

  const collaborationPolicyNode = (
    <CollaborationPolicyControl
      runtimeType={conversation.type}
      delegationPolicy={collaborationPolicy.delegationPolicy}
      decisionPolicy={collaborationPolicy.decisionPolicy}
      onChange={onCollaborationPolicyChange}
      compact
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
      saved,
    );
    if (!heal) return;
    void (async () => {
      const selected = {
        ...heal.provider,
        use_model: heal.use_model,
      } as TProviderWithModel;
      const execution_model_pool = buildConversationModelPool(
        { provider_id: heal.provider.id, model: heal.use_model },
        activeCollaborators,
      );
      if (!execution_model_pool) return;
      const ok = await ipcBridge.conversation.update.invoke({
        conversation_id: conversation.id,
        updates: { model: selected, execution_model_pool, execution_template_id: null },
      });
      if (ok) {
        setSelectedCollaborationTemplate(null);
        void saveNomiDefaultModel(heal.provider.id, heal.use_model);
        Message.info(
          t('conversation.chat.modelHealedToDefault', {
            model: heal.use_model,
          }),
        );
      }
    })();
    // Re-evaluate when the conversation or provider list changes.
  }, [
    activeCollaborators,
    conversation.id,
    conversation.model?.id,
    conversation.model?.use_model,
    healProviders,
    healGetAvailable,
    t,
  ]);

  const workspaceEnabled = Boolean(conversation.extra?.workspace);
  const { info: presetPresetInfo } = usePresetInfo(conversation);

  const chatLayoutProps = {
    title: conversation.name,
    siderTitle: sliderTitle,
    sider: <ChatSlider conversation={conversation} />,
    headerExtra: (
      <div className='flex items-center gap-8px'>
        {/* The collaboration canvas lives beside the mounted conversation; the
            header keeps the existing capability controls. */}
        <CronJobManager
          conversation_id={conversation.id}
          cron_job_id={conversation.cron_job_id}
          hasCronSkill={hasLoadedSkill(conversation, 'cron')}
        />
      </div>
    ),
    workspaceEnabled,
    workspacePath: conversation.extra?.workspace,
    isTemporaryWorkspace: (conversation.extra as { is_temporary_workspace?: boolean } | undefined)
      ?.is_temporary_workspace,
    backend: 'nomi' as const,
    preset: presetPresetInfo ?? undefined,
  };

  return (
    <NomiConversationLayout
      conversation={conversation}
      chatLayoutProps={chatLayoutProps}
      modelSelection={modelSelection}
      collaboratorSelectorNode={collaboratorSelectorNode}
      collaborationPolicyNode={collaborationPolicyNode}
      presetPresetName={presetPresetInfo?.name}
    />
  );
};

const ChatConversation: React.FC<{
  conversation?: TChatConversation;
  hideSendBox?: boolean;
}> = ({ conversation, hideSendBox }) => {
  const { t } = useTranslation();
  const workspaceEnabled = Boolean(conversation?.extra?.workspace);

  const isNomiConversation = conversation?.type === 'nomi';

  // Use the shared hook for preset snapshot information in ACP/Codex
  // conversations.
  const acpConversation = isNomiConversation ? undefined : conversation;
  const { info: presetPresetInfo, isLoading: isLoadingPreset } = usePresetInfo(acpConversation);

  const conversationAgentName = (conversation?.extra as { agent_name?: string } | undefined)?.agent_name;
  const presetDisplayName = presetPresetInfo?.name || conversationAgentName;

  const conversationNode = useMemo(() => {
    if (!conversation || isNomiConversation) return null;
    switch (conversation.type) {
      case 'acp': {
        const extra = conversation.extra as {
          backend?: string;
          current_model_id?: string;
        };
        return (
          <AcpChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace}
            backend={extra.backend || 'claude'}
            initialModelId={extra.current_model_id}
            session_mode={conversation.extra?.session_mode}
            agent_name={presetDisplayName}
            cron_job_id={conversation.cron_job_id}
            hideSendBox={hideSendBox}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
            loadedMcpStatuses={
              (conversation.extra as { mcp_statuses?: IConversationMcpStatus[] } | undefined)?.mcp_statuses
            }
          ></AcpChat>
        );
      }
      case 'openclaw-gateway':
        return (
          <OpenClawChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            cron_job_id={conversation.cron_job_id}
            hideSendBox={hideSendBox}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
          />
        );
      case 'nanobot':
        return (
          <NanobotChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            cron_job_id={conversation.cron_job_id}
            hideSendBox={hideSendBox}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
          />
        );
      case 'remote':
        return (
          <RemoteChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            cron_job_id={conversation.cron_job_id}
            hideSendBox={hideSendBox}
            loadedSkills={(conversation.extra as { skills?: string[] } | undefined)?.skills}
          />
        );
      default:
        return null;
    }
  }, [conversation, isNomiConversation, presetDisplayName, hideSendBox]);

  const sliderTitle = useMemo(() => {
    return (
      <div className='flex items-center justify-between'>
        <span className='text-16px font-bold text-t-primary'>{t('conversation.workspace.title')}</span>
      </div>
    );
  }, [t]);

  const workspaceExtraTabs = useMemo(
    () =>
      conversation?.extra?.workspace
        ? [
            {
              key: 'conversation-terminals',
              title: t('terminal.conversationPanel.tab'),
              icon: <Terminal size={18} />,
              content: <ConversationTerminalPanel conversationId={conversation.id} />,
            },
          ]
        : [],
    [conversation?.id, conversation?.extra?.workspace, t],
  );

  const isRetainedAttemptTranscript = Boolean(
    conversation?.execution_step_id || conversation?.execution_attempt_id,
  );

  // An Attempt Conversation is immutable execution audit data, not a second
  // ordinary chat entry point. Direct/history navigation therefore uses the
  // same read-only projection as the collaboration canvas; decisions, steer,
  // retry and lifecycle changes remain AgentExecution commands.
  if (conversation && isRetainedAttemptTranscript) {
    return (
      <ExecutionProvider conversation={conversation}>
        <ExecutionConversationLayout
          title={conversation.name}
          conversation_id={conversation.id}
          hideAdvancedControls
          disableRename
          siderTitle={sliderTitle}
          sider={<ChatSlider conversation={conversation} extraTabs={workspaceExtraTabs} />}
          workspaceEnabled={Boolean(conversation.extra?.workspace)}
          workspacePath={conversation.extra?.workspace}
          isTemporaryWorkspace={
            (conversation.extra as { is_temporary_workspace?: boolean } | undefined)
              ?.is_temporary_workspace
          }
          workspaceExtraTabs={workspaceExtraTabs}
        >
          <ReadOnlyConversationView
            conversation={conversation}
            agent_name={(conversation.extra as { agent_name?: string } | undefined)?.agent_name}
          />
        </ExecutionConversationLayout>
      </ExecutionProvider>
    );
  }

  if (conversation && conversation.type === 'nomi') {
    // 桌面伙伴的专属会话（单会话契约）走受限面板：保留锁定模型/隐藏高级控制/强制 yolo/
    // Companion sessions use a fixed workspace and restricted controls.
    // Configuration controls remain limited for companion sessions, while
    // linked execution progress and lifecycle state stay visible.
    if (conversation.extra?.companion_session) {
      return (
        <ExecutionProvider conversation={conversation}>
          <CompanionChatPanel
            key={conversation.id}
            conversation={conversation}
            extraTabs={workspaceExtraTabs}
          />
        </ExecutionProvider>
      );
    }
    return (
      <ExecutionProvider conversation={conversation}>
        <NomiConversationPanel key={conversation.id} conversation={conversation} sliderTitle={sliderTitle} />
      </ExecutionProvider>
    );
  }

  // If preset snapshot info exists, use its logo/name. While loading, avoid
  // falling back prematurely; otherwise use the backend logo.
  const chatLayoutProps = presetPresetInfo
    ? {
        preset: presetPresetInfo,
      }
    : isLoadingPreset
      ? {} // Still loading custom agents; avoid showing the backend logo prematurely.
      : {
          backend:
            conversation?.type === 'acp'
              ? conversation?.extra?.backend
              : // `nomi` conversations are handled by the early return above and can
                // never reach this branch, so the chain starts at non-ACP types.
                conversation?.type === 'openclaw-gateway'
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
            cron_job_id={conversation.cron_job_id}
            hasCronSkill={hasLoadedSkill(conversation, 'cron')}
          />
        </div>
      )}
    </div>
  );

  const layout = (
    <ExecutionConversationLayout
      title={conversation?.name}
      {...chatLayoutProps}
      headerExtra={headerExtraNode}
      siderTitle={sliderTitle}
      sider={<ChatSlider conversation={conversation} extraTabs={workspaceExtraTabs} />}
      workspaceEnabled={workspaceEnabled}
      workspacePath={conversation?.extra?.workspace}
      isTemporaryWorkspace={
        (conversation?.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
      }
      conversation_id={conversation?.id}
      workspaceExtraTabs={workspaceExtraTabs}
    >
      {conversationNode}
    </ExecutionConversationLayout>
  );

  if (!conversation) {
    return (
      <ChatLayout
        title={undefined}
        {...chatLayoutProps}
        headerExtra={headerExtraNode}
        siderTitle={sliderTitle}
        sider={<ChatSlider conversation={undefined} />}
        workspaceEnabled={workspaceEnabled}
      >
        {conversationNode}
      </ChatLayout>
    );
  }

  return <ExecutionProvider conversation={conversation}>{layout}</ExecutionProvider>;
};

export default ChatConversation;
