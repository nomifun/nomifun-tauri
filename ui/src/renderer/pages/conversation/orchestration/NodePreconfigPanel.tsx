/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@arco-design/web-react';
import { Brain, SettingOne, Write } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TModelRef, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import NomiSelect from '@/renderer/components/base/NomiSelect';
import { decodePair, encodePair, useModelRange } from '@/renderer/pages/orchestrator/useModelRange';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';

/** Sentinel select value = "跟随自动路由" (clears the per-task model override). */
const FOLLOW_AUTO = '__follow_auto__';

/** Case-insensitive substring match on an option's text (mirrors OrchestratorComposer). */
const filterByLabel = (input: string, option: React.ReactNode): boolean => {
  const children = (option as React.ReactElement<{ children?: React.ReactNode }>)?.props?.children;
  return String(children ?? '')
    .toLowerCase()
    .includes(input.toLowerCase());
};

type NodePreconfigPanelProps = {
  runId: string;
  /** The node being configured (from the live run detail). */
  task: TRunTask;
  /** true for a settled (done/failed/…) node — copy says the change needs a 重跑;
   * false for a pending node — it takes effect at dispatch. */
  settled: boolean;
  /** Re-pull the run so the canvas + this panel reflect the saved config. */
  onSaved: () => void | Promise<void>;
};

/**
 * NodePreconfigPanel — a node's 启动前配置台 (migration 025). Lets the user, BEFORE a
 * node runs, (1) override which model this node uses (any available provider×model,
 * not just the run's frozen fleet) and (2) pre-inject a 预置要求 appended to the
 * node's worker brief (separate from the planner-written spec). Rendered by
 * {@link ProjectedWorkerView} as the pending node's body, and as a collapsible
 * "重跑配置" for a settled node. Persists via `orchestrator.runs.setTaskConfig` (a
 * FULL replace — clearing the model select restores auto-routing).
 */
const NodePreconfigPanel: React.FC<NodePreconfigPanelProps> = ({ runId, task, settled, onSaved }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const { providers, getAvailableModels, formatModelLabel, hasModels } = useModelRange();

  // Prefill from the task's persisted override (both provider + model present) or
  // the follow-auto sentinel.
  const initialModel =
    task.override_provider_id && task.override_model
      ? encodePair({ provider_id: task.override_provider_id, model: task.override_model })
      : FOLLOW_AUTO;
  const initialPreset = task.preset_prompt ?? '';

  const [modelValue, setModelValue] = useState<string>(initialModel);
  const [preset, setPreset] = useState<string>(initialPreset);
  const [saving, setSaving] = useState(false);

  const dirty = modelValue !== initialModel || preset !== initialPreset;

  const save = async () => {
    if (saving || !dirty) return;
    setSaving(true);
    try {
      const ref: TModelRef | null = modelValue !== FOLLOW_AUTO ? decodePair(modelValue) : null;
      await ipcBridge.orchestrator.runs.setTaskConfig.invoke({
        run_id: runId,
        task_id: task.id,
        updates: {
          override_provider_id: ref?.provider_id,
          override_model: ref?.model,
          preset_prompt: preset.trim() || undefined,
        },
      });
      message.success(
        settled
          ? t('orchestrator.run.preconfig.savedRerun', {
              defaultValue: '已保存;该节点已运行过，点「重跑」用新配置重跑',
            })
          : t('orchestrator.run.preconfig.savedPending', { defaultValue: '已保存，启动时自动生效' })
      );
      await onSaved();
    } catch (e) {
      message.error(
        t('orchestrator.run.preconfig.saveError', { defaultValue: '保存失败：{{error}}', error: String(e) })
      );
    } finally {
      setSaving(false);
    }
  };

  const sectionClass =
    'flex flex-col gap-8px rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-1)] px-14px py-12px';
  const labelClass = 'inline-flex items-center gap-6px text-12px font-600 text-[var(--color-text-1)]';

  return (
    <div className='mx-auto w-full max-w-560px flex flex-col gap-14px px-16px py-20px'>
      {msgCtx}

      {/* Header */}
      <div className='flex items-center gap-8px'>
        <span
          className='flex size-28px shrink-0 items-center justify-center rounded-9px'
          style={{
            color: 'rgb(var(--primary-6))',
            background: 'color-mix(in srgb, rgb(var(--primary-6)) 12%, transparent)',
            border: '1px solid color-mix(in srgb, rgb(var(--primary-6)) 28%, transparent)',
          }}
        >
          <SettingOne theme='outline' size='16' strokeWidth={3} className='line-height-0' />
        </span>
        <div className='min-w-0 flex flex-col'>
          <span className='text-14px font-600 text-[var(--color-text-1)] truncate'>
            {t('orchestrator.run.preconfig.title', { defaultValue: '启动前配置' })}
          </span>
          <span className='text-11px text-[var(--color-text-3)] truncate'>
            {settled
              ? t('orchestrator.run.preconfig.subtitleSettled', { defaultValue: '调整该节点的模型与要求，重跑后生效' })
              : t('orchestrator.run.preconfig.subtitlePending', {
                  defaultValue: '为该节点指定模型、预置要求；启动时自动生效',
                })}
          </span>
        </div>
      </div>

      {/* Model override */}
      <div className={sectionClass}>
        <span className={labelClass}>
          <Brain theme='outline' size='13' strokeWidth={3} className='line-height-0 text-[rgb(var(--primary-6))]' />
          {t('orchestrator.run.preconfig.modelLabel', { defaultValue: '指定模型' })}
        </span>
        {hasModels ? (
          <NomiSelect
            value={modelValue}
            onChange={(v) => setModelValue(v as string)}
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
        <span className='text-11px leading-16px text-[var(--color-text-3)]'>
          {t('orchestrator.run.preconfig.modelHint', {
            defaultValue: '可选任意已配置的模型，不受本次编排创建时所选模型池限制。',
          })}
        </span>
      </div>

      {/* Preset requirement */}
      <div className={sectionClass}>
        <span className={labelClass}>
          <Write theme='outline' size='13' strokeWidth={3} className='line-height-0 text-[rgb(var(--primary-6))]' />
          {t('orchestrator.run.preconfig.presetLabel', { defaultValue: '预置要求' })}
        </span>
        <Input.TextArea
          value={preset}
          onChange={(v) => setPreset(v)}
          autoSize={{ minRows: 3, maxRows: 10 }}
          placeholder={t('orchestrator.run.preconfig.presetPlaceholder', {
            defaultValue: '在此写下该节点执行时必须遵守的额外要求/偏好（会追加到该节点的输入，与任务描述分开）。',
          })}
        />
      </div>

      {/* Footer: hint + save */}
      <div className='flex items-center justify-between gap-12px'>
        <span className='text-11px leading-16px text-[var(--color-text-3)]'>
          {settled
            ? t('orchestrator.run.preconfig.footerSettled', { defaultValue: '保存后点击顶部「重跑」用新配置重跑该节点' })
            : t('orchestrator.run.preconfig.footerPending', { defaultValue: '该节点启动时自动应用此配置' })}
        </span>
        <div
          role='button'
          tabIndex={0}
          aria-disabled={!dirty || saving}
          onClick={dirty && !saving ? () => void save() : undefined}
          onKeyDown={(e) => {
            if ((e.key === 'Enter' || e.key === ' ') && dirty && !saving) {
              e.preventDefault();
              void save();
            }
          }}
          className={[
            'shrink-0 inline-flex items-center gap-6px rounded-9px px-16px py-7px text-13px font-600 leading-none transition-all duration-150 select-none',
            dirty && !saving
              ? 'cursor-pointer text-white'
              : 'cursor-not-allowed text-[var(--color-text-3)]',
          ].join(' ')}
          style={
            dirty && !saving
              ? { background: 'rgb(var(--primary-6))' }
              : { background: 'var(--color-fill-2)', border: '1px solid var(--color-border-2)' }
          }
        >
          {saving
            ? t('orchestrator.run.preconfig.saving', { defaultValue: '保存中…' })
            : t('orchestrator.run.preconfig.save', { defaultValue: '保存配置' })}
        </div>
      </div>
    </div>
  );
};

export default NodePreconfigPanel;
