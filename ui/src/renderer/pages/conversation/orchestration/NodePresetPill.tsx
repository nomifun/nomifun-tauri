/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dropdown, Input } from '@arco-design/web-react';
import { Write } from '@icon-park/react';
import { iconColors } from '@/renderer/styles/colors';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import composerStyles from '@/renderer/pages/orchestrator/orchestratorComposer.module.css';

type NodePresetPillProps = {
  /** Current persisted preset — seeds the draft and drives the dirty/highlight state. */
  initialPreset: string;
  /** true for a settled (done/failed/…) node: the save takes effect on the next 重跑,
   * not automatically at start — the success toast says which. */
  settled: boolean;
  /** Persist the trimmed preset. THROWS on failure so the pill can toast. The parent
   * merges this against the node's other config (model override) — this pill never
   * touches the model, so it cannot wipe it. */
  onApply: (preset: string) => Promise<void>;
  className?: string;
};

/**
 * NodePresetPill — the 预置要求 (a per-node requirement APPENDED to the worker brief on
 * the node's NEXT 重跑/dispatch, distinct from chatting live with the worker) folded
 * into a compact composer-toolbar pill. Clicking opens a popover with the textarea +
 * an inline save. Purely presentational: it reports the edited preset via `onApply`
 * and never calls the persistence layer itself, so the parent can merge it atomically
 * with the model override (setTaskConfig is a full replace). The pill highlights when
 * a preset is set so it stays discoverable.
 */
const NodePresetPill: React.FC<NodePresetPillProps> = ({ initialPreset, settled, onApply, className }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();
  const [open, setOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [preset, setPreset] = useState(initialPreset);

  // Seed the draft from the persisted value on each OPEN transition only — never while
  // the popover is open, so a background task refresh can't clobber in-progress typing.
  const onVisibleChange = (v: boolean) => {
    if (v) setPreset(initialPreset);
    setOpen(v);
  };

  const dirty = preset !== initialPreset;
  const hasPreset = initialPreset.trim().length > 0;

  const save = async () => {
    if (saving || !dirty) return;
    setSaving(true);
    try {
      await onApply(preset.trim());
      message.success(
        settled
          ? t('orchestrator.run.preconfig.savedRerun', {
              defaultValue: '已保存；该节点已运行过，点「重跑」用新配置重跑',
            })
          : t('orchestrator.run.preconfig.savedPending', { defaultValue: '已保存，启动时自动生效' })
      );
      setOpen(false);
    } catch (e) {
      message.error(t('orchestrator.run.preconfig.saveError', { defaultValue: '保存失败：{{error}}', error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  const panel = (
    <div className={composerStyles.composerPopover}>
      <div className='flex flex-col gap-10px'>
        <div className='flex items-center gap-8px'>
          <Write theme='outline' size='14' fill='rgb(var(--primary-6))' className='shrink-0' />
          <span className={composerStyles.composerPopoverTitle}>
            {t('orchestrator.run.preconfig.presetLabel', { defaultValue: '预置要求' })}
          </span>
        </div>
        <Input.TextArea
          value={preset}
          onChange={setPreset}
          autoSize={{ minRows: 3, maxRows: 10 }}
          placeholder={t('orchestrator.run.preconfig.presetPlaceholder', {
            defaultValue: '在此写下该节点执行时必须遵守的额外要求/偏好（会追加到该节点的输入，与任务描述分开）。',
          })}
        />
        <div className='flex items-center justify-between gap-8px'>
          <span className={composerStyles.composerHint}>
            {t('orchestrator.run.preconfig.presetPillHint', { defaultValue: '影响该节点下次重跑/启动' })}
          </span>
          <Button type='primary' size='mini' loading={saving} disabled={!dirty} onClick={() => void save()}>
            {t('orchestrator.run.preconfig.save', { defaultValue: '保存配置' })}
          </Button>
        </div>
      </div>
    </div>
  );

  return (
    <>
      {msgCtx}
      <Dropdown trigger='click' popupVisible={open} onVisibleChange={onVisibleChange} droplist={panel} position='tr'>
        <Button
          className={`sendbox-model-btn ${className ?? ''}`}
          shape='round'
          size='small'
          aria-label={t('orchestrator.run.preconfig.presetLabel', { defaultValue: '预置要求' })}
        >
          <span className='flex items-center gap-6px min-w-0'>
            <Write
              theme='outline'
              size='14'
              className='shrink-0'
              fill={hasPreset ? 'rgb(var(--primary-6))' : iconColors.secondary}
            />
            <span className='truncate' style={hasPreset ? { color: 'rgb(var(--primary-6))' } : undefined}>
              {t('orchestrator.run.preconfig.presetPill', { defaultValue: '预置要求' })}
            </span>
          </span>
        </Button>
      </Dropdown>
    </>
  );
};

export default NodePresetPill;
