/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * GeneratorNode — P0 shell.
 *
 * This intentionally renders only the *frame* of a generation card: the mode
 * glyph, a prompt summary, a status badge, and a disabled "configure" affordance.
 * The real card (model/param panel, `@`-mentions, run/cancel wiring, batch grid,
 * result thumbnails, continuous-edit chain) lands in M7.
 *
 * ── M7 integration slots ─────────────────────────────────────────────────────
 *  - Replace the `<PromptSummary>` / footer region with the editable param panel.
 *  - Wire the footer button to submit a `creation_tasks` job (see `api.createTask`)
 *    and reflect live status via `data.status` / `data.taskId`.
 *  - Render `data.resultAssetIds` (batch group) with expand/collapse + set-primary
 *    using `data.batch`.
 *  - `data.mentions` powers the `@asset` reference chips.
 * All of the above only *reads/writes* `data` through `api.updateNodeData`, so the
 * canvas contract here is stable.
 */

import React, { useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { DeleteFour, MagicWand, Pic, Text, VideoTwo } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import type { GeneratorFlowNode } from '../model';
import { KIND_META } from '../model';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';

const MODE_ICON: Record<string, React.ReactNode> = {
  image: <Pic theme='outline' size={14} strokeWidth={3} />,
  text: <Text theme='outline' size={14} strokeWidth={3} />,
  video: <VideoTwo theme='outline' size={14} strokeWidth={3} />,
};

function statusTone(status: string): { color: string; bg: string } {
  switch (status) {
    case 'running':
    case 'queued':
      return { color: 'rgb(var(--primary-6))', bg: 'rgba(var(--primary-6),0.12)' };
    case 'success':
      return { color: 'rgb(var(--success-6))', bg: 'rgba(var(--success-6),0.12)' };
    case 'error':
      return { color: 'rgb(var(--danger-6))', bg: 'rgba(var(--danger-6),0.12)' };
    default:
      return { color: 'var(--color-text-3)', bg: 'var(--color-fill-2)' };
  }
}

function GeneratorNodeImpl({ id, data, selected }: NodeProps<GeneratorFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);

  const mode = typeof data.mode === 'string' ? data.mode : 'image';
  const status = typeof data.status === 'string' ? data.status : 'idle';
  const prompt = typeof data.prompt === 'string' ? data.prompt : '';
  const tone = statusTone(status);
  const modeLabel = t(`workshopCanvas.node.generator.mode.${mode}`, { defaultValue: mode });
  const statusLabel = t(`workshopCanvas.node.generator.status.${status}`, { defaultValue: status });

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.generator.minWidth} minHeight={KIND_META.generator.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected}>
          <NodeHandles />
          <div className='flex h-full w-full flex-col'>
            {/* Header */}
            <div className='flex items-center gap-8px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-12px py-9px'>
              <span
                className='flex h-24px w-24px items-center justify-center rounded-7px text-[rgb(var(--primary-6))]'
                style={{ background: 'rgba(var(--primary-6),0.12)' }}
              >
                <MagicWand theme='outline' size={14} strokeWidth={3} />
              </span>
              <span className='flex items-center gap-4px text-13px font-700 text-[var(--color-text-1)]'>
                {MODE_ICON[mode]}
                {t('workshopCanvas.node.generator.title', { mode: modeLabel, defaultValue: '{{mode}}生成' })}
              </span>
              <span
                className='ml-auto rounded-full px-7px py-2px text-10px font-600 leading-none'
                style={{ color: tone.color, background: tone.bg }}
              >
                {statusLabel}
              </span>
            </div>

            {/* Prompt summary */}
            <div className='min-h-0 flex-1 overflow-hidden px-12px py-10px'>
              {prompt.trim() ? (
                <p className='m-0 line-clamp-3 text-12px leading-[1.55] text-[var(--color-text-2)]'>{prompt}</p>
              ) : (
                <p className='m-0 text-12px leading-[1.55] text-[var(--color-text-3)]'>
                  {t('workshopCanvas.node.generator.promptEmpty', { defaultValue: '尚未填写提示词' })}
                </p>
              )}
              {Array.isArray(data.resultAssetIds) && data.resultAssetIds.length > 0 && (
                <span className='mt-8px inline-flex items-center gap-4px rounded-full bg-[var(--color-fill-2)] px-8px py-2px text-10px text-[var(--color-text-3)]'>
                  {t('workshopCanvas.node.generator.results', {
                    count: data.resultAssetIds.length,
                    defaultValue: '{{count}} 张产物',
                  })}
                </span>
              )}
            </div>

            {/* Footer — disabled config affordance (M7 wires this up) */}
            <div className='border-t border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-b-0 px-12px py-9px'>
              <div
                title={t('workshopCanvas.node.generator.comingSoonHint', {
                  defaultValue: '生成能力即将接通（M7）',
                })}
                className='flex w-full items-center justify-center gap-6px rounded-8px border border-dashed border-[var(--color-border-3)] px-10px py-7px text-12px text-[var(--color-text-3)] cursor-not-allowed select-none'
              >
                <MagicWand theme='outline' size={13} strokeWidth={3} />
                {t('workshopCanvas.node.generator.configure', { defaultValue: '配置生成' })}
              </div>
            </div>
          </div>
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(GeneratorNodeImpl);
