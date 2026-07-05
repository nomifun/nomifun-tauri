/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import { Comment } from '@icon-park/react';
import type { TModelRef, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import composerStyles from '@/renderer/pages/orchestrator/orchestratorComposer.module.css';
import NodeModelPill from './NodeModelPill';
import NodePresetPill from './NodePresetPill';

type NodeConfigBarProps = {
  /** Live task — drives the pills' current-selection display. */
  task: TRunTask;
  /** Persist the model override (`null` = follow auto). Throws on failure. */
  onApplyModel: (ref: TModelRef | null) => Promise<void>;
  /** Persist the preset requirement. Throws on failure. */
  onApplyPreset: (preset: string) => Promise<void>;
};

/**
 * NodeConfigBar — the PENDING node's 启动前配置 surface. A pending node has no worker
 * conversation (hence no NomiSendBox to reuse), so we mirror the composer's bottom
 * toolbar as a slim bar carrying the SAME two controls (model + 预置要求 pills) a
 * settled node gets INSIDE its real composer. No text-send affordance — there is
 * nothing to chat with until the node is dispatched; both pills persist through the
 * parent's atomic config write and are applied automatically when the DAG reaches
 * this node. The pills sit inside `.composerToolbar` so they inherit the same
 * transparent-pill skin as a real composer's toolbar.
 */
const NodeConfigBar: React.FC<NodeConfigBarProps> = ({ task, onApplyModel, onApplyPreset }) => {
  const { t } = useTranslation();
  return (
    <div className='flex flex-1 min-h-0 flex-col'>
      {/* Empty-ish body — explains the node hasn't started + what the bar does. */}
      <div className='flex flex-1 min-h-0 flex-col items-center justify-center gap-10px px-20px text-center'>
        <span
          className='flex size-48px items-center justify-center rd-14px'
          style={{
            color: 'rgb(var(--primary-6))',
            background: 'color-mix(in srgb, rgb(var(--primary-6)) 12%, transparent)',
          }}
        >
          <Comment theme='outline' size='24' strokeWidth={3} />
        </span>
        <div className='text-14px font-600 text-[var(--color-text-1)]'>
          {t('orchestrator.run.transcript.notStarted', { defaultValue: '该 agent 尚未开始' })}
        </div>
        <div className='max-w-360px text-12px leading-18px text-[var(--color-text-3)]'>
          {t('orchestrator.run.preconfig.pendingHint', {
            defaultValue: '为该节点指定模型、预置要求；启动时自动生效。',
          })}
        </div>
      </div>

      {/* Composer-shaped config bar — same pills + transparent skin as a real composer. */}
      <div className='shrink-0 border-t border-solid border-[var(--color-border-2)] px-16px py-12px'>
        <div className={composerStyles.composerToolbar}>
          <NodeModelPill task={task} onApply={onApplyModel} />
          <NodePresetPill initialPreset={task.preset_prompt ?? ''} settled={false} onApply={onApplyPreset} />
        </div>
      </div>
    </div>
  );
};

export default NodeConfigBar;
