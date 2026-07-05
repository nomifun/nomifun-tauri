/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { AgentMetadata } from '@/renderer/utils/model/agentTypes';
import classNames from 'classnames';
import NomiModal from '@/renderer/components/base/NomiModal';
import { useAgents } from '@/renderer/hooks/agent/useAgents';
import { useContainerWidth } from '@/renderer/hooks/ui/useContainerWidth';
import { Button, Typography } from '@arco-design/web-react';
import { IconRefresh } from '@arco-design/web-react/icon';
import { Home, Plus } from '@icon-park/react';
import React, { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router-dom';
import AgentCard from './AgentCard';
import { AgentHubModal } from './AgentHubModal';
import InlineAgentEditor, { type CustomAgentDraft } from './InlineAgentEditor';
import { getAgentKey } from '@/renderer/pages/guid/hooks/agentSelectionUtils';
import { openExternalUrl } from '@/renderer/utils/platform';
import { useNomiQuickStart } from '@/renderer/hooks/agent/useNomiQuickStart';
import { SUPPORTED_AGENTS, type SupportedAgent } from './supportedAgents';

/**
 * 卡片网格按「内容容器实际宽度」自动定列，而非视口断点 —— 模型管理内容面板
 * 被一次 rail + 二级 ContentSider 占去宽度，视口宽 ≠ 面板可用宽，用 md:/lg:/xl:
 * 视口断点会在窄面板下给出过多列数把卡片挤到裁剪。auto-fill 让列数随容器缩放。
 * Card grids auto-fit columns to the actual container width (not viewport
 * breakpoints): the model-hub pane is narrower than the viewport, so viewport
 * md:/lg:/xl: over-columns and clips cards on a narrow pane.
 */
const CARD_GRID_COLS = 'repeat(auto-fill, minmax(min(168px, 100%), 1fr))';

const LocalAgents: React.FC = () => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { ref, width } = useContainerWidth<HTMLDivElement>();
  // 横排 banner 需要足够宽度容纳「图标+标题+描述 | 按钮」，否则纵排。
  // 首帧 width=0 视为窄(纵排)，纵排永不裁剪，仅首帧后若够宽再转横排。
  const bannerWide = width >= 520;
  const [hubModalVisible, setHubModalVisible] = useState(false);

  // Single fetch for all agents; both detected and custom lists are derived from it.
  const {
    agents: allAgents,
    revalidate: mutateAgents,
    refreshCustomAgents,
  } = useAgents();
  const [refreshingDetection, setRefreshingDetection] = useState(false);

  const handleRefreshDetection = useCallback(async () => {
    setRefreshingDetection(true);
    try {
      await refreshCustomAgents();
    } catch (err) {
      console.error('refresh local agent detection failed:', err);
    } finally {
      setRefreshingDetection(false);
    }
  }, [refreshCustomAgents]);

  const detectedAgents = allAgents.filter((a) => a.agent_type !== 'remote' && a.agent_source !== 'custom');

  const customAgents: AgentMetadata[] = allAgents.filter((a) => a.agent_source === 'custom');

  // Diff the curated catalog against what was detected so users can see — and
  // install — the agents NomiFun supports but that aren't on this machine yet.
  const installedBackends = new Set(detectedAgents.map((a) => a.backend || a.agent_type));
  const notInstalledAgents = SUPPORTED_AGENTS.filter(
    (s) => !installedBackends.has(s.backend) && (Boolean(s.website) || s.installHint.trim().length > 0)
  );

  const { start: startNomiInstall } = useNomiQuickStart();
  const [installingBackend, setInstallingBackend] = useState<string | null>(null);

  const handleOneClickInstall = useCallback(
    async (agent: SupportedAgent) => {
      setInstallingBackend(agent.backend);
      const hint = agent.installHint || t('settings.agentManagement.installHintUnknown');
      await startNomiInstall({
        name: t('settings.agentManagement.installConversationName', { name: agent.name }),
        prompt: t('settings.agentManagement.installPrompt', {
          name: agent.name,
          hint,
          binary: agent.binary,
          website: agent.website || '',
        }),
      });
      setInstallingBackend(null);
    },
    [startNomiInstall, t]
  );

  const [editorVisible, setEditorVisible] = useState(false);
  const [editingAgent, setEditingAgent] = useState<AgentMetadata | null>(null);

  const handleSaveCustomAgent = useCallback(
    async (draft: CustomAgentDraft) => {
      const body = {
        name: draft.name,
        command: draft.command,
        icon: draft.icon,
        args: draft.args,
        env: draft.env,
        advanced: draft.advanced,
      };
      try {
        if (editingAgent) {
          await ipcBridge.acpConversation.updateCustomAgent.invoke({ id: editingAgent.id, ...body });
        } else {
          await ipcBridge.acpConversation.createCustomAgent.invoke(body);
        }
        await mutateAgents();
        setEditorVisible(false);
        setEditingAgent(null);
      } catch (err) {
        // Surface backend rejection (e.g. cli_not_found / acp_init_failed) without crashing.
        console.error('save custom agent failed:', err);
      }
    },
    [editingAgent, mutateAgents]
  );

  const handleDeleteCustomAgent = useCallback(
    async (agentId: string) => {
      try {
        await ipcBridge.acpConversation.deleteCustomAgent.invoke({ id: agentId });
        await mutateAgents();
      } catch (err) {
        console.error('delete custom agent failed:', err);
      }
    },
    [mutateAgents]
  );

  const handleToggleCustomAgent = useCallback(
    async (agentId: string, enabled: boolean) => {
      try {
        await ipcBridge.acpConversation.setAgentEnabled.invoke({ id: agentId, enabled });
        await mutateAgents();
      } catch (err) {
        console.error('toggle custom agent failed:', err);
      }
    },
    [mutateAgents]
  );

  // Nomi first among detected agents
  const nomiAgent = detectedAgents?.find((a) => a.agent_type === 'nomi' || a.backend === 'nomi');
  const otherDetected = detectedAgents?.filter((a) => a.agent_type !== 'nomi' && a.backend !== 'nomi') ?? [];

  const openCustomAgentEditor = useCallback(() => {
    setEditingAgent(null);
    setEditorVisible(true);
  }, []);

  const goToChatWithAgent = useCallback(
    (agent: AgentMetadata) => {
      navigate('/guid', { state: { selectedAgentKey: getAgentKey(agent) } });
    },
    [navigate]
  );

  return (
    <div ref={ref} className='flex flex-col gap-8px py-16px'>
      <div className='flex flex-wrap items-center gap-x-6px gap-y-2px px-16px text-12px text-t-secondary'>
        <span>{t('settings.agentManagement.localAgentsDescription')} </span>
        <Button
          type='text'
          size='mini'
          className='!h-auto !p-0 !align-baseline !text-12px !font-normal !text-primary-6 hover:!text-primary-7 hover:!underline underline-offset-2'
          onClick={openCustomAgentEditor}
        >
          {t('settings.agentManagement.detectCustomAgent')}
        </Button>
        <Button
          type='text'
          size='mini'
          icon={<IconRefresh />}
          loading={refreshingDetection}
          data-testid='btn-refresh-local-agents'
          className='!h-auto !p-0 !align-baseline !text-12px !font-normal !text-primary-6 hover:!text-primary-7 hover:!underline underline-offset-2'
          onClick={() => void handleRefreshDetection()}
        >
          {t('settings.agentManagement.refreshDetection')}
        </Button>
      </div>

      {process.env.NODE_ENV === 'development' && (
        <div className='px-16px mt-8px'>
          <div
            className={classNames(
              'flex gap-14px rounded-16px border border-solid border-[rgba(var(--primary-6),0.18)] bg-[rgba(var(--primary-6),0.06)] p-16px',
              bannerWide ? 'flex-row items-center justify-between' : 'flex-col'
            )}
          >
            <div className='flex items-center gap-12px'>
              <div className='flex h-40px w-40px items-center justify-center leading-none rounded-12px border border-solid border-[rgba(var(--primary-6),0.12)] bg-[rgba(var(--primary-6),0.10)] text-primary-6 shadow-[inset_0_1px_0_rgba(255,255,255,0.28)]'>
                <Home theme='outline' size='20' strokeWidth={2} className='block' />
              </div>
              <div className='min-w-0'>
                <Typography.Text className='mb-4px block text-15px font-medium text-t-primary'>
                  {t('settings.agentManagement.installFromMarket')}
                </Typography.Text>
                <Typography.Text className='block text-12px leading-18px text-t-secondary'>
                  {t('settings.agentManagement.discoverMoreAgents')}
                </Typography.Text>
              </div>
            </div>

            <Button
              type='primary'
              size='small'
              icon={<Plus size='14' />}
              className={classNames('!rounded-10px shrink-0', bannerWide && '!min-w-144px')}
              onClick={() => setHubModalVisible(true)}
            >
              {t('settings.agentManagement.installFromMarket')}
            </Button>
          </div>
        </div>
      )}

      {/* Installed Agents section */}
      <div className='px-16px mt-8px'>
        <Typography.Text className='text-12px font-medium text-t-secondary mb-4px block'>
          {t('settings.agentManagement.installed')}
        </Typography.Text>
      </div>
      <div className='grid gap-10px px-16px' style={{ gridTemplateColumns: CARD_GRID_COLS }}>
        {nomiAgent && (
          <AgentCard
            type='detected'
            agent={nomiAgent}
            onGoToChat={() => goToChatWithAgent(nomiAgent)}
          />
        )}
        {otherDetected.map((agent) => (
          <AgentCard
            key={agent.backend || agent.agent_type}
            type='detected'
            agent={agent}
            onGoToChat={() => goToChatWithAgent(agent)}
          />
        ))}
      </div>
      {(!detectedAgents || detectedAgents.length === 0) && (
        <Typography.Text type='secondary' className='block px-16px py-16px text-center text-12px'>
          {t('settings.agentManagement.localAgentsEmpty')}
        </Typography.Text>
      )}

      {/* Not-installed Agents section — discover & install supported agents */}
      {notInstalledAgents.length > 0 && (
        <>
          <div className='px-16px mt-16px'>
            <Typography.Text className='text-12px font-medium text-t-secondary mb-2px block'>
              {t('settings.agentManagement.notInstalled')}
            </Typography.Text>
            <Typography.Text className='block text-11px leading-16px text-t-tertiary'>
              {t('settings.agentManagement.notInstalledDesc')}
            </Typography.Text>
          </div>
          <div className='grid gap-10px px-16px' style={{ gridTemplateColumns: CARD_GRID_COLS }}>
            {notInstalledAgents.map((agent) => (
              <AgentCard
                key={agent.backend}
                type='installable'
                agent={{ backend: agent.backend, name: agent.name, website: agent.website }}
                installing={installingBackend === agent.backend}
                onOneClickInstall={
                  agent.installHint.trim().length > 0 ? () => void handleOneClickInstall(agent) : undefined
                }
                onManualInstall={agent.website ? () => void openExternalUrl(agent.website as string) : undefined}
              />
            ))}
          </div>
        </>
      )}

      {/* Custom Agents section */}
      {(editorVisible || (customAgents && customAgents.length > 0)) && (
        <div className='px-16px mt-16px'>
          <Typography.Text className='text-12px font-medium text-t-secondary mb-4px block'>
            {t('settings.agentManagement.customAgents', { defaultValue: 'Custom Agents' })}
          </Typography.Text>
        </div>
      )}

      <NomiModal
        visible={editorVisible}
        onCancel={() => {
          setEditorVisible(false);
          setEditingAgent(null);
        }}
        header={{
          title: editingAgent
            ? t('settings.agentManagement.editCustomAgent')
            : t('settings.agentManagement.detectCustomAgent'),
          showClose: true,
        }}
        footer={null}
        style={{ maxWidth: '92vw', borderRadius: 16 }}
        contentStyle={{
          background: 'var(--dialog-fill-0)',
          borderRadius: 16,
          padding: '20px 24px 16px',
          overflow: 'auto',
        }}
      >
        {/* Conditional mount + key unmounts the editor on close so the
            next `创建自定义 Agent` click always starts from a blank form.
            The inner useEffect([agent]) only resets when the `agent`
            reference changes; two consecutive `null` values would not
            retrigger it. */}
        {editorVisible && (
          <InlineAgentEditor
            key={editingAgent?.id ?? 'new'}
            agent={editingAgent}
            onSave={(agent) => void handleSaveCustomAgent(agent)}
            onCancel={() => {
              setEditorVisible(false);
              setEditingAgent(null);
            }}
          />
        )}
      </NomiModal>

      <div className='flex flex-col gap-4px px-0'>
        {customAgents?.map((agent) => (
          <AgentCard
            key={agent.id}
            type='custom'
            agent={agent}
            onGoToChat={() => goToChatWithAgent(agent)}
            onEdit={() => {
              setEditingAgent(agent);
              setEditorVisible(true);
            }}
            onDelete={() => void handleDeleteCustomAgent(agent.id)}
            onToggle={(enabled) => void handleToggleCustomAgent(agent.id, enabled)}
          />
        ))}
      </div>

      {hubModalVisible && <AgentHubModal visible={hubModalVisible} onCancel={() => setHubModalVisible(false)} />}
    </div>
  );
};

export default LocalAgents;
