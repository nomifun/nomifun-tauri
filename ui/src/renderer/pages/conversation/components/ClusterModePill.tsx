/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Popover, Switch } from '@arco-design/web-react';
import { EveryUser } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';

/**
 * ClusterModePill —「agent 集群」的会话内开关 pill（需求1/5）。
 *
 * 挂在 nomi composer 工具条（协作模型选择器旁），popover 内两枚开关：
 *  - **agent 集群**：写 `extra.agent_cluster_mode`。开启后主 agent 对每个任务都
 *    刻意评估是否用多 agent 集群协作；太简单则先说明原因再直接作答（后端
 *    CLUSTER_MODE_HINT 消费）。
 *  - **节点审批模式**：写 `extra.orchestrator_approval_mode`（'manual' | 'auto'）。
 *    manual 下集群节点遇关键决策会挂起提问（画布/进度条亮提问徽标），由用户
 *    进入该节点作答；auto（默认全授权）节点自行判断。建 run 时读取生效。
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

  const content = (
    <div className='flex w-260px flex-col gap-12px py-2px'>
      <div className='flex items-start justify-between gap-12px'>
        <div className='flex min-w-0 flex-col gap-2px'>
          <span className='text-13px font-600 text-t-primary'>
            {t('conversation.cluster.toggleTitle', { defaultValue: 'agent 集群' })}
          </span>
          <span className='text-11px leading-16px text-t-tertiary'>
            {t('conversation.cluster.toggleDesc', {
              defaultValue: '主 agent 对每个任务刻意评估是否拆给多个独立 agent 并行交付；太简单会说明原因后直接作答。',
            })}
          </span>
        </div>
        <Switch size='small' checked={cluster} onChange={toggleCluster} />
      </div>
      <div className='flex items-start justify-between gap-12px'>
        <div className='flex min-w-0 flex-col gap-2px'>
          <span className='text-13px font-600 text-t-primary'>
            {t('conversation.cluster.approvalTitle', { defaultValue: '节点审批模式' })}
          </span>
          <span className='text-11px leading-16px text-t-tertiary'>
            {t('conversation.cluster.approvalDesc', {
              defaultValue: '节点遇关键决策时挂起向你提问，由你进入该节点作答后继续；关闭则全授权由各节点自行判断。',
            })}
          </span>
        </div>
        <Switch size='small' checked={approval} onChange={toggleApproval} />
      </div>
    </div>
  );

  return (
    <Popover content={content} trigger='click' position='top' unmountOnExit>
      <button
        type='button'
        className='nomi-sendbox-model-btn'
        aria-label={t('conversation.cluster.pillAria', { defaultValue: 'agent 集群设置' })}
        style={
          cluster
            ? {
                color: 'rgb(var(--primary-6))',
                background: 'color-mix(in srgb, rgb(var(--primary-6)) 10%, transparent)',
              }
            : undefined
        }
      >
        <EveryUser theme='outline' size='14' strokeWidth={3} />
        <span>{t('conversation.cluster.pill', { defaultValue: '集群' })}</span>
        {approval && cluster && (
          <span
            className='rd-full px-4px text-10px font-600 leading-14px'
            style={{
              color: 'var(--warning)',
              background: 'color-mix(in srgb, var(--warning) 14%, transparent)',
            }}
          >
            {t('conversation.cluster.approvalShort', { defaultValue: '审批' })}
          </span>
        )}
      </button>
    </Popover>
  );
};

export default ClusterModePill;
