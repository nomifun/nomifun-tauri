/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * LoopNode — the loop controller card (M8, the workshop's core differentiator).
 *
 * Wire it to a generator card (loop → generator, drawn as an accented dashed
 * edge) and it drives that card `count` times in one click. Per round it advances
 * a window over the target's upstream images (`start` / `batch`), injects a
 * count line into the prompt (`{i}`), and fans results out to the target's right
 * on a per-round grid row. Serial or parallel (rolling window of 3), with a live
 * progress ring, success / failure tallies, and mid-run abort.
 *
 * All heavy lifting lives in `generation/loop/` (the coordinator + a remount-safe
 * run registry); this component is the control surface only.
 */

import React, { useMemo, useState } from 'react';
import { useParams } from 'react-router-dom';
import { type NodeProps, useNodesData, useStore } from '@xyflow/react';
import { AlignTextLeft, Cycle, DeleteFour, ListNumbers, Pause, Play, SortAmountDown } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import type { LoopFlowNode, WorkshopFlowNode } from '../model';
import { KIND_META } from '../model';
import type { WorkshopGeneratorMode, WorkshopLoopMode } from '../../types';
import { useGeneratorModels } from '../../generation/useGeneratorModels';
import type { ModelOption } from '../../generation/genTypes';
import {
  LOOP_BATCH_MAX,
  LOOP_BATCH_MIN,
  LOOP_COUNT_MAX,
  LOOP_COUNT_MIN,
  LOOP_START_MIN,
  injectCount,
  readLoopConfig,
  useLoopRunner,
} from '../../generation/loop';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';

const Stepper: React.FC<{
  label: string;
  icon: React.ReactNode;
  value: number;
  min: number;
  max: number;
  disabled?: boolean;
  onChange: (v: number) => void;
}> = ({ label, icon, value, min, max, disabled, onChange }) => {
  const step = (delta: number): void => {
    if (disabled) return;
    onChange(Math.min(max, Math.max(min, value + delta)));
  };
  const btn =
    'grid h-20px w-20px place-items-center rounded-5px cursor-pointer select-none text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)] transition-colors';
  return (
    <div className='flex flex-col gap-3px'>
      <span className='flex items-center gap-3px text-9px font-600 uppercase tracking-wide text-[var(--color-text-3)]'>
        <span className='flex h-11px w-11px items-center justify-center'>{icon}</span>
        {label}
      </span>
      <div className={['nodrag inline-flex items-center gap-2px rounded-7px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-3px py-2px', disabled ? 'opacity-55' : ''].join(' ')}>
        <div role='button' tabIndex={0} onClick={() => step(-1)} onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && step(-1)} className={btn}>
          −
        </div>
        <span className='min-w-22px text-center text-12px font-700 tabular-nums text-[var(--color-text-1)]'>{value}</span>
        <div role='button' tabIndex={0} onClick={() => step(1)} onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && step(1)} className={btn}>
          +
        </div>
      </div>
    </div>
  );
};

/** A circular progress ring showing completed/total with a centred label. */
const ProgressRing: React.FC<{ completed: number; total: number; running: boolean; label: string }> = ({
  completed,
  total,
  running,
  label,
}) => {
  const r = 15;
  const c = 2 * Math.PI * r;
  const pct = total > 0 ? completed / total : 0;
  return (
    <span className='relative inline-flex h-38px w-38px items-center justify-center'>
      <svg width={38} height={38} className={running ? 'animate-[spin_6s_linear_infinite]' : ''}>
        <circle cx={19} cy={19} r={r} fill='none' stroke='var(--color-fill-3)' strokeWidth={3} />
        <circle
          cx={19}
          cy={19}
          r={r}
          fill='none'
          stroke='rgb(var(--primary-6))'
          strokeWidth={3}
          strokeLinecap='round'
          strokeDasharray={c}
          strokeDashoffset={c * (1 - pct)}
          transform='rotate(-90 19 19)'
          style={{ transition: 'stroke-dashoffset 0.3s ease' }}
        />
      </svg>
      <span className='absolute text-9px font-700 tabular-nums text-[var(--color-text-1)]'>{label}</span>
    </span>
  );
};

function LoopNodeImpl({ id, data, selected }: NodeProps<LoopFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const { id: canvasId = '' } = useParams<{ id: string }>();
  const [hover, setHover] = useState(false);

  const config = useMemo(() => readLoopConfig(data), [data]);

  // The generator card this loop drives (loop → generator edge).
  const targetId = useStore((s) => {
    for (const e of s.edges) {
      if (e.source !== id) continue;
      const n = s.nodeLookup.get(e.target);
      if (n && n.type === 'generator') return e.target;
    }
    return null;
  });
  const targetData = useNodesData<WorkshopFlowNode>(targetId ?? '');
  const targetMode: WorkshopGeneratorMode =
    (targetData?.data as { mode?: WorkshopGeneratorMode } | undefined)?.mode ?? 'image';

  const models = useGeneratorModels(targetMode);
  const effectiveModel = useMemo<ModelOption | null>(() => {
    const td = targetData?.data as { providerId?: string; model?: string } | undefined;
    const explicit = models.flat.find((m) => m.providerId === td?.providerId && m.model === td?.model);
    return explicit ?? models.flat[0] ?? null;
  }, [models.flat, targetData]);

  const { progress, running, canRun, start, stop } = useLoopRunner({
    loopId: id,
    targetId,
    canvasId,
    config,
    model: effectiveModel,
  });

  const set = (patch: Record<string, unknown>): void => api.updateNodeData(id, patch);

  const previewRound = progress.activeRound ?? 1;
  const injected = injectCount(config.countTemplate, previewRound);
  const ringLabel = running ? `${progress.completed}/${config.count}` : `${config.count}`;

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.loop.minWidth} minHeight={KIND_META.loop.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected}>
          {/* Loop drives the target — source handle only. */}
          <NodeHandles sides='source' />

          {/* Header + progress ring. */}
          <div className='flex shrink-0 items-center gap-8px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-11px py-8px'>
            <span
              className='flex h-22px w-22px items-center justify-center rounded-6px text-white'
              style={{ background: 'linear-gradient(135deg, #06b6d4, #0891b2)' }}
            >
              <Cycle theme='outline' size={13} strokeWidth={3} />
            </span>
            <div className='flex min-w-0 flex-1 flex-col'>
              <span className='text-12px font-700 text-[var(--color-text-1)]'>
                {t('workshopCanvas.node.loop.title', { defaultValue: '循环' })}
              </span>
              <span className='truncate text-9px text-[var(--color-text-3)]'>
                {targetId
                  ? t(`workshopCanvas.node.loop.targetMode.${targetMode}`, { defaultValue: targetMode })
                  : t('workshopCanvas.node.loop.noTarget', { defaultValue: '拖右侧锚点连到生成卡片' })}
              </span>
            </div>
            <ProgressRing completed={progress.completed} total={config.count} running={running} label={ringLabel} />
          </div>

          {/* Body: config + injected preview + tallies. */}
          <div className='nowheel flex min-h-0 flex-1 flex-col gap-9px overflow-y-auto px-11px py-9px'>
            <div className='grid grid-cols-3 gap-6px'>
              <Stepper
                label={t('workshopCanvas.node.loop.count', { defaultValue: '次数' })}
                icon={<Cycle theme='outline' size={11} strokeWidth={3} />}
                value={config.count}
                min={LOOP_COUNT_MIN}
                max={LOOP_COUNT_MAX}
                disabled={running}
                onChange={(v) => set({ count: v })}
              />
              <Stepper
                label={t('workshopCanvas.node.loop.start', { defaultValue: '起始' })}
                icon={<ListNumbers theme='outline' size={11} strokeWidth={3} />}
                value={config.start}
                min={LOOP_START_MIN}
                max={999}
                disabled={running}
                onChange={(v) => set({ start: v })}
              />
              <Stepper
                label={t('workshopCanvas.node.loop.batch', { defaultValue: '批次' })}
                icon={<SortAmountDown theme='outline' size={11} strokeWidth={3} />}
                value={config.batch}
                min={LOOP_BATCH_MIN}
                max={LOOP_BATCH_MAX}
                disabled={running}
                onChange={(v) => set({ batch: v })}
              />
            </div>

            {/* Serial / parallel mode. */}
            <div className='flex items-center gap-3px rounded-8px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-2px'>
              {(['serial', 'parallel'] as WorkshopLoopMode[]).map((m) => {
                const active = config.loopMode === m;
                return (
                  <div
                    key={m}
                    role='button'
                    tabIndex={0}
                    onClick={() => !running && set({ loopMode: m })}
                    onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && !running && set({ loopMode: m })}
                    className={[
                      'nodrag flex-1 rounded-6px py-4px text-center text-11px font-600 cursor-pointer transition-colors select-none',
                      running ? 'opacity-55' : '',
                      active ? 'bg-[rgb(var(--primary-6))] text-white' : 'text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)]',
                    ].join(' ')}
                  >
                    {t(`workshopCanvas.node.loop.mode.${m}`, { defaultValue: m === 'serial' ? '串行' : '并发' })}
                  </div>
                );
              })}
            </div>

            {/* Count-injection template + live preview. */}
            <div className='flex flex-col gap-3px'>
              <span className='flex items-center gap-3px text-9px font-600 uppercase tracking-wide text-[var(--color-text-3)]'>
                <AlignTextLeft theme='outline' size={11} strokeWidth={3} />
                {t('workshopCanvas.node.loop.template', { defaultValue: '计数注入' })}
              </span>
              <input
                value={config.countTemplate}
                disabled={running}
                onChange={(e) => set({ countTemplate: e.target.value })}
                onKeyDown={(e) => e.stopPropagation()}
                placeholder={t('workshopCanvas.node.loop.templatePlaceholder', { defaultValue: '现在生成第 {i} 张' })}
                className='nodrag w-full rounded-7px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-8px py-5px text-11px text-[var(--color-text-1)] outline-none focus:border-[rgb(var(--primary-6))] disabled:opacity-55'
              />
              {injected && (
                <span className='truncate rounded-6px bg-[rgba(var(--primary-6),0.08)] px-7px py-4px text-10px text-[rgb(var(--primary-6))]'>
                  {t('workshopCanvas.node.loop.previewLabel', { defaultValue: '本轮注入' })}: {injected}
                </span>
              )}
            </div>

            {/* Tallies + failed rounds. */}
            {(running || progress.completed > 0) && (
              <div className='flex flex-wrap items-center gap-5px'>
                <span className='inline-flex items-center gap-3px rounded-full bg-[rgba(var(--success-6),0.12)] px-7px py-2px text-10px font-600 text-[rgb(var(--success-6))]'>
                  {t('workshopCanvas.node.loop.success', { defaultValue: '成功' })} {progress.success}
                </span>
                {progress.failed.length > 0 && (
                  <span className='inline-flex items-center gap-3px rounded-full bg-[rgba(var(--danger-6),0.12)] px-7px py-2px text-10px font-600 text-[rgb(var(--danger-6))]'>
                    {t('workshopCanvas.node.loop.failed', { defaultValue: '失败' })} {progress.failed.join(', ')}
                  </span>
                )}
                {progress.status === 'done' && (
                  <span className='text-10px text-[var(--color-text-3)]'>{t('workshopCanvas.node.loop.done', { defaultValue: '已完成' })}</span>
                )}
              </div>
            )}
          </div>

          {/* Footer: run / stop. */}
          <div className='shrink-0 border-t border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-b-0 px-11px py-8px'>
            {running ? (
              <div
                role='button'
                tabIndex={0}
                onClick={(e) => {
                  e.stopPropagation();
                  stop();
                }}
                onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && stop()}
                className='nodrag flex w-full items-center justify-center gap-6px rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-10px py-7px text-12px font-600 text-[var(--color-text-2)] cursor-pointer transition-colors hover:border-[rgb(var(--danger-6))] hover:text-[rgb(var(--danger-6))]'
              >
                <Pause theme='outline' size={13} strokeWidth={3} />
                {t('workshopCanvas.node.loop.stop', { defaultValue: '中止' })}
                <span className='text-10px font-400 opacity-70'>· {progress.completed}/{config.count}</span>
              </div>
            ) : (
              <div
                role='button'
                tabIndex={canRun ? 0 : -1}
                onClick={(e) => {
                  e.stopPropagation();
                  if (canRun) start();
                }}
                onKeyDown={(e) => {
                  if ((e.key === 'Enter' || e.key === ' ') && canRun) {
                    e.preventDefault();
                    start();
                  }
                }}
                title={
                  canRun
                    ? undefined
                    : !targetId
                      ? t('workshopCanvas.node.loop.noTarget', { defaultValue: '拖右侧锚点连到生成卡片' })
                      : t('workshopCanvas.node.loop.noModel', { defaultValue: '目标卡片无可用模型' })
                }
                className={[
                  'nodrag flex w-full items-center justify-center gap-6px rounded-9px px-10px py-7px text-12px font-700 transition-all select-none',
                  canRun
                    ? 'bg-[rgb(var(--primary-6))] text-white cursor-pointer hover:opacity-92 shadow-[0_4px_14px_rgba(var(--primary-6),0.32)]'
                    : 'bg-[var(--color-fill-3)] text-[var(--color-text-3)] cursor-not-allowed',
                ].join(' ')}
              >
                <Play theme='outline' size={13} strokeWidth={3} />
                {t('workshopCanvas.node.loop.run', { defaultValue: '运行循环' })}
              </div>
            )}
          </div>
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(LoopNodeImpl);
