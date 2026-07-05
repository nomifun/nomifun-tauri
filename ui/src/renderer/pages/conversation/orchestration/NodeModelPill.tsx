/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown } from '@arco-design/web-react';
import { Brain, Down } from '@icon-park/react';
import type { TModelRef, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { decodePair, encodePair, useModelRange } from '@/renderer/pages/orchestrator/useModelRange';
import { iconColors } from '@/renderer/styles/colors';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import composerStyles from '@/renderer/pages/orchestrator/orchestratorComposer.module.css';

/** Sentinel select value = "跟随自动路由" (clears the per-task model override). */
const FOLLOW_AUTO = '__follow_auto__';

/** Case-insensitive substring match on an option's text (mirrors OrchestratorComposer). */
const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '')
    .toLowerCase()
    .includes(input.toLowerCase());
};

type NodeModelPillProps = {
  /** Live task — for the current-selection display only (writes go through onApply). */
  task: TRunTask;
  /** Persist the model override (`null` = follow auto-routing / clear). THROWS on
   * failure so the pill can toast. The parent merges this against the preset — this
   * pill never touches the preset, so it cannot wipe it. */
  onApply: (ref: TModelRef | null) => Promise<void>;
  className?: string;
};

/**
 * NodeModelPill — a single-model override pill for a PENDING node. A pending node has
 * no worker conversation yet, so there is no NomiSendBox model selector to reuse (a
 * settled node reuses that selector, whose pick is written through as the override).
 * Lists ANY configured provider×model — not just the run's frozen fleet — and carries
 * the「跟随自动路由」clear option. Purely presentational: it reports the chosen model
 * via `onApply` and never persists directly.
 */
const NodeModelPill: React.FC<NodeModelPillProps> = ({ task, onApply, className }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const { providers, getAvailableModels, formatModelLabel, hasModels } = useModelRange();
  const [open, setOpen] = useState(false);

  const value =
    task.override_provider_id && task.override_model
      ? encodePair({ provider_id: task.override_provider_id, model: task.override_model })
      : FOLLOW_AUTO;

  const pillLabel =
    value === FOLLOW_AUTO
      ? t('orchestrator.run.preconfig.followAuto', { defaultValue: '跟随自动路由（不指定）' })
      : (task.override_model ?? '');

  const persist = async (next: string) => {
    try {
      await onApply(next !== FOLLOW_AUTO ? decodePair(next) : null);
      setOpen(false);
    } catch (e) {
      message.error(t('orchestrator.run.preconfig.saveError', { defaultValue: '保存失败：{{error}}', error: String(e) }));
    }
  };

  const panel = (
    <div className={composerStyles.composerPopover}>
      <div className='flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <Brain theme='outline' size='14' fill='rgb(var(--primary-6))' className='shrink-0' />
          <span className={composerStyles.composerPopoverTitle}>
            {t('orchestrator.run.preconfig.modelLabel', { defaultValue: '指定模型' })}
          </span>
        </div>
        {hasModels ? (
          <NomiSelect
            value={value}
            onChange={(v) => void persist(v as string)}
            showSearch
            filterOption={filterByLabel}
            className='w-full'
          >
            <NomiSelect.Option value={FOLLOW_AUTO}>
              {t('orchestrator.run.preconfig.followAuto', { defaultValue: '跟随自动路由（不指定）' })}
            </NomiSelect.Option>
            {providers.map((p) => (
              <NomiSelect.OptGroup key={p.id} label={p.name || p.platform}>
                {getAvailableModels(p).map((m) => {
                  const ref: TModelRef = { provider_id: p.id, model: m };
                  return (
                    <NomiSelect.Option key={encodePair(ref)} value={encodePair(ref)}>
                      {formatModelLabel(p, m)}
                    </NomiSelect.Option>
                  );
                })}
              </NomiSelect.OptGroup>
            ))}
          </NomiSelect>
        ) : (
          <span className='text-12px leading-18px text-[rgb(var(--warning-6))]'>
            {t('orchestrator.run.preconfig.noModels', { defaultValue: '暂无可用模型，请先在「模型」里配置 provider。' })}
          </span>
        )}
        <span className={composerStyles.composerHint}>
          {t('orchestrator.run.preconfig.modelHint', {
            defaultValue: '可选任意已配置的模型，不受本次编排创建时所选模型池限制。',
          })}
        </span>
      </div>
    </div>
  );

  return (
    <>
      {msgCtx}
      <Dropdown trigger='click' popupVisible={open} onVisibleChange={setOpen} droplist={panel} position='tr'>
        <Button
          className={`sendbox-model-btn ${className ?? ''}`}
          shape='round'
          size='small'
          aria-label={t('orchestrator.run.preconfig.modelLabel', { defaultValue: '指定模型' })}
        >
          <span className='flex items-center gap-6px min-w-0'>
            <Brain theme='outline' size='14' className='shrink-0' fill={iconColors.secondary} />
            <span className='truncate max-w-[160px]'>{pillLabel}</span>
            <Down theme='outline' size='12' className='shrink-0' fill={iconColors.secondary} />
          </span>
        </Button>
      </Dropdown>
    </>
  );
};

export default NodeModelPill;
