/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Popover, Switch } from '@arco-design/web-react';
import { EveryUser } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';

/**
 * ClusterModePill —「agent 集群」的会话内图标开关（需求1/5）。
 *
 * 挂在 nomi composer 工具条的权限旁边，popover 内保留两枚开关：
 *  - **agent 集群**：写 `extra.agent_cluster_mode`。
 *  - **节点审批模式**：写 `extra.orchestrator_approval_mode`（'manual' | 'auto'）。
 *
 * 写回走 `conversation.update` 的 extra 顶层浅合并（只覆盖本键，同级键保留——
 * 与 orchestrator_model_range 的写法同源）。本地乐观态 + conversation 刷新回灌。
 */
const ClusterModePill: React.FC<{ conversation: TChatConversation }> = ({ conversation }) => {
  const { t } = useTranslation();
  const extra = (conversation.extra ?? {}) as {
    agent_cluster_mode?: boolean;
    orchestrator_approval_mode?: string;
  };
  const [cluster, setCluster] = useState<boolean>(Boolean(extra.agent_cluster_mode));
  const [approval, setApproval] = useState<boolean>(extra.orchestrator_approval_mode === 'manual');

  // conversation 刷新（listChanged → 重取）回灌乐观态，保证多入口写入后一致。
  useEffect(() => {
    setCluster(Boolean(extra.agent_cluster_mode));
    setApproval(extra.orchestrator_approval_mode === 'manual');
  }, [conversation.id, extra.agent_cluster_mode, extra.orchestrator_approval_mode]);

  const persist = async (patch: Record<string, unknown>) => {
    try {
      await ipcBridge.conversation.update.invoke({
        id: conversation.id,
        updates: { extra: patch as TChatConversation['extra'] },
      });
    } catch (err) {
      console.error('[ClusterModePill] persist cluster settings failed', err);
    }
  };

  const toggleCluster = (next: boolean) => {
    setCluster(next);
    void persist({ agent_cluster_mode: next });
  };
  const toggleApproval = (next: boolean) => {
    setApproval(next);
    // 只认 'manual'；关闭写 'auto'（而非删键——浅合并无删除语义）。
    void persist({ orchestrator_approval_mode: next ? 'manual' : 'auto' });
  };

  const ariaLabel = t('conversation.cluster.pillAria', { defaultValue: 'agent 集群设置' });

  const content = (
    <div className='flex w-220px flex-col gap-10px py-2px'>
      <div className='flex items-start justify-between gap-12px'>
        <div className='flex min-w-0 flex-col gap-2px'>
          <span className='text-13px font-600 text-t-primary'>
            {t('conversation.cluster.toggleTitle', { defaultValue: '集群' })}
          </span>
          <span className='text-11px leading-16px text-t-tertiary'>
            {t('conversation.cluster.toggleDesc', {
              defaultValue: '需要时拆给多个 agent 并行处理。',
            })}
          </span>
        </div>
        <Switch size='small' checked={cluster} onChange={toggleCluster} />
      </div>
      <div className='flex items-start justify-between gap-12px'>
        <div className='flex min-w-0 flex-col gap-2px'>
          <span className='text-13px font-600 text-t-primary'>
            {t('conversation.cluster.approvalTitle', { defaultValue: '节点确认' })}
          </span>
          <span className='text-11px leading-16px text-t-tertiary'>
            {t('conversation.cluster.approvalDesc', {
              defaultValue: '关键决策先暂停，等你确认。',
            })}
          </span>
        </div>
        <Switch size='small' checked={approval} onChange={toggleApproval} />
      </div>
    </div>
  );

  return (
    <Popover content={content} trigger='click' position='top' unmountOnExit>
      <Button
        type='text'
        shape='circle'
        size='small'
        className={`sendbox-cluster-pill ${cluster ? 'sendbox-cluster-pill--active' : ''}`}
        aria-label={ariaLabel}
        aria-pressed={cluster}
        title={ariaLabel}
        data-testid='cluster-mode-pill'
      >
        <EveryUser theme='outline' size='15' fill='currentColor' strokeWidth={3} />
        {cluster && <span className='sendbox-cluster-pill__dot' aria-hidden='true' />}
      </Button>
    </Popover>
  );
};

export default ClusterModePill;
