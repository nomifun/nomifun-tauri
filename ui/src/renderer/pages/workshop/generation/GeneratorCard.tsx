/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * GeneratorCard — the interactive body of a generation node (M7). Composes the
 * mode switch, model picker, prompt (+`@`-mentions), input summary, per-mode
 * params, run/cancel control, status + error surfaces, and the result view with
 * its continuous-edit chain. All state lives in the node's `data`, mutated via
 * the canvas `updateNodeData` (history + autosave), so the card is fully
 * serialisable and survives reloads.
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useParams } from 'react-router-dom';
import { useReactFlow } from '@xyflow/react';
import { useTranslation } from 'react-i18next';
import { Info, MagicWand, Pause, Pic, Play, Refresh, Text, VideoTwo } from '@icon-park/react';
import SegmentedTabs, { type SegmentedTabItem } from '@renderer/components/base/SegmentedTabs';
import { useCanvasNode } from '../canvas/CanvasNodeContext';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import type { WorkshopGeneratorMode, WorkshopGeneratorNodeData, WorkshopGeneratorStatus } from '../types';
import { CARD_SIZE, FACTORY_BOX } from './genConstants';
import type { GenMode, ModelOption } from './genTypes';
import { useGeneratorModels } from './useGeneratorModels';
import { useGenerationRun } from './useGenerationRun';
import { spawnContinueCard, spawnTextNode } from './spawn';
import ModelPicker from './ModelPicker';
import PromptField from './PromptField';
import ParamControls from './ParamControls';
import InputSummary from './InputSummary';
import ResultView from './ResultView';
import { isLocalZImageModel } from './localZImage';

const MODE_META: Record<GenMode, { icon: React.ReactNode }> = {
  image: { icon: <Pic theme='outline' size={12} strokeWidth={3} /> },
  video: { icon: <VideoTwo theme='outline' size={12} strokeWidth={3} /> },
  text: { icon: <Text theme='outline' size={12} strokeWidth={3} /> },
};

function statusTone(status: WorkshopGeneratorStatus): { color: string; bg: string } {
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

export interface GeneratorCardProps {
  id: string;
  data: WorkshopGeneratorNodeData;
}

const GeneratorCard: React.FC<GeneratorCardProps> = ({ id, data }) => {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();
  const { id: canvasId = '' } = useParams<{ id: string }>();

  const mode: GenMode = data.mode ?? 'image';
  const status: WorkshopGeneratorStatus = data.status ?? 'idle';
  const prompt = data.prompt ?? '';
  const mentions = useMemo(() => data.mentions ?? [], [data.mentions]);
  const params = useMemo(() => data.params ?? {}, [data.params]);
  const results = useMemo(() => data.resultAssetIds ?? [], [data.resultAssetIds]);
  const running = status === 'queued' || status === 'running';

  const models = useGeneratorModels(mode);
  const effectiveModel = useMemo<ModelOption | null>(() => {
    const explicit = models.flat.find((m) => m.providerId === data.providerId && m.model === data.model);
    return explicit ?? models.flat[0] ?? null;
  }, [models.flat, data.providerId, data.model]);
  const localZImage = isLocalZImageModel(effectiveModel);

  const { run, cancel } = useGenerationRun({
    nodeId: id,
    canvasId,
    data,
    effectiveModel,
    updateNodeData: api.updateNodeData as (nodeId: string, patch: Partial<WorkshopGeneratorNodeData>) => void,
  });

  // ── Data mutators ────────────────────────────────────────────────────────────

  const setMode = useCallback((m: WorkshopGeneratorMode) => api.updateNodeData(id, { mode: m }), [api, id]);
  const setModel = useCallback(
    (opt: ModelOption) => api.updateNodeData(id, { providerId: opt.providerId, model: opt.model }),
    [api, id]
  );
  const setPrompt = useCallback((text: string) => api.updateNodeData(id, { prompt: text }), [api, id]);
  const addMention = useCallback(
    (ref: string) => {
      if (mentions.includes(ref)) return;
      api.updateNodeData(id, { mentions: [...mentions, ref] });
    },
    [api, id, mentions]
  );
  const removeMention = useCallback(
    (ref: string) => api.updateNodeData(id, { mentions: mentions.filter((m) => m !== ref) }),
    [api, id, mentions]
  );
  const setParams = useCallback(
    (patch: Record<string, unknown>) => api.updateNodeData(id, { params: { ...params, ...patch } }),
    [api, id, params]
  );

  const continueEdit = useCallback(
    (instruction: string) => {
      const card = rf.getNode(id);
      if (!card) return;
      spawnContinueCard(rf, card, {
        instruction,
        providerId: effectiveModel?.providerId,
        model: effectiveModel?.model,
        mode: mode === 'video' ? 'video' : 'image',
      });
    },
    [rf, id, effectiveModel, mode]
  );

  const toTextNode = useCallback(
    (content: string) => {
      const card = rf.getNode(id);
      if (card) spawnTextNode(rf, card, content);
    },
    [rf, id]
  );

  // ── Grow the freshly-minted box to a comfortable card size, once. ──────────────

  const sizedRef = useRef(false);
  useEffect(() => {
    if (sizedRef.current) return;
    sizedRef.current = true;
    const node = rf.getNode(id);
    if (!node) return;
    if (Math.round(node.width ?? 0) === FACTORY_BOX.width && Math.round(node.height ?? 0) === FACTORY_BOX.height) {
      api.resizeNode(id, CARD_SIZE[mode] ?? CARD_SIZE.image);
    }
    // Mount-only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Auto-run mid-chain cards (mask repaint / continuous edit) once ready. ──────

  const autoRanRef = useRef(false);
  useEffect(() => {
    if (autoRanRef.current || !data.autoRun || !effectiveModel) return;
    autoRanRef.current = true;
    api.updateNodeData(id, { autoRun: false });
    run();
  }, [data.autoRun, effectiveModel, api, id, run]);

  // ── Render ─────────────────────────────────────────────────────────────────────

  const [errExpanded, setErrExpanded] = useState(false);
  const tone = statusTone(status);

  const modeItems: SegmentedTabItem[] = (['image', 'video', 'text'] as GenMode[]).map((m) => ({
    key: m,
    label: t(`workshopGeneration.mode.${m}`, { defaultValue: m }),
    icon: MODE_META[m].icon,
  }));

  const runLabel =
    results.length > 0
      ? t('workshopGeneration.run.regenerate', { defaultValue: '重新生成' })
      : t('workshopGeneration.run.run', { defaultValue: '生成' });
  const runningLabel =
    status === 'queued'
      ? t('workshopGeneration.status.queued', { defaultValue: '排队中' })
      : t('workshopGeneration.status.running', { defaultValue: '生成中' });

  return (
    <div className='flex h-full w-full flex-col'>
      {/* Header */}
      <div className='shrink-0 border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-11px py-9px'>
        <div className='mb-8px flex items-center gap-8px'>
          <span
            className='flex h-22px w-22px items-center justify-center rounded-6px text-[rgb(var(--primary-6))]'
            style={{ background: 'rgba(var(--primary-6),0.12)' }}
          >
            <MagicWand theme='outline' size={13} strokeWidth={3} />
          </span>
          <span className='text-13px font-700 text-[var(--color-text-1)]'>
            {t('workshopGeneration.card.title', { defaultValue: '生成卡片' })}
          </span>
          {status !== 'idle' && (
            <span
              className='ml-auto inline-flex items-center gap-4px rounded-full px-7px py-2px text-10px font-600 leading-none'
              style={{ color: tone.color, background: tone.bg }}
            >
              {running && (
                <span className='h-8px w-8px animate-spin rounded-full border-[1.5px] border-solid border-transparent border-t-current' />
              )}
              {t(`workshopGeneration.status.${status}`, { defaultValue: status })}
            </span>
          )}
        </div>
        <div className='nodrag mb-8px'>
          <SegmentedTabs items={modeItems} activeKey={mode} onChange={(k) => setMode(k as WorkshopGeneratorMode)} size='sm' block />
        </div>
        <ModelPicker
          mode={mode}
          providerId={data.providerId ?? effectiveModel?.providerId}
          model={data.model ?? effectiveModel?.model}
          onChange={setModel}
        />
      </div>

      {/* Body */}
      <div className='nowheel flex min-h-0 min-w-0 flex-1 flex-col gap-10px overflow-y-auto px-11px py-10px'>
        {status === 'success' && results.length > 0 && (
          <ResultView
            mode={mode}
            resultAssetIds={results}
            batch={data.batch}
            onContinueEdit={localZImage ? undefined : continueEdit}
            onToTextNode={toTextNode}
          />
        )}
        <PromptField value={prompt} mode={mode} selfId={id} onChange={setPrompt} onAddMention={addMention} />
        <InputSummary selfId={id} mentions={mentions} onRemoveMention={removeMention} />
        <ParamControls mode={mode} model={effectiveModel} params={params} onChange={setParams} />
      </div>

      {/* Footer */}
      <div className='shrink-0 border-t border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-b-0 px-11px py-9px'>
        {status === 'error' && data.errorMessage && (
          <div
            role='button'
            tabIndex={0}
            onClick={() => setErrExpanded((v) => !v)}
            onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && setErrExpanded((v) => !v)}
            className='nodrag mb-8px flex items-start gap-6px rounded-8px border border-solid border-[rgba(var(--danger-6),0.35)] bg-[rgba(var(--danger-6),0.08)] px-9px py-6px cursor-pointer'
          >
            <Info theme='outline' size={13} strokeWidth={3} className='mt-1px shrink-0 text-[rgb(var(--danger-6))]' />
            <span className={['flex-1 text-11px leading-[1.5] text-[rgb(var(--danger-6))]', errExpanded ? '' : 'line-clamp-2'].join(' ')}>
              {data.errorMessage}
            </span>
          </div>
        )}
        {running ? (
          <div
            role='button'
            tabIndex={0}
            onClick={(e) => {
              e.stopPropagation();
              cancel();
            }}
            onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && cancel()}
            className='nodrag flex w-full box-border items-center justify-center gap-6px rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-10px py-8px text-12px font-600 text-[var(--color-text-2)] cursor-pointer transition-colors hover:border-[rgb(var(--danger-6))] hover:text-[rgb(var(--danger-6))]'
          >
            <Pause theme='outline' size={13} strokeWidth={3} />
            {t('workshopGeneration.run.cancel', { defaultValue: '取消' })}
            <span className='text-10px font-400 opacity-70'>· {runningLabel}</span>
          </div>
        ) : (
          <div
            role='button'
            tabIndex={effectiveModel ? 0 : -1}
            onClick={(e) => {
              e.stopPropagation();
              if (effectiveModel) run();
            }}
            onKeyDown={(e) => {
              if ((e.key === 'Enter' || e.key === ' ') && effectiveModel) {
                e.preventDefault();
                run();
              }
            }}
            title={effectiveModel ? undefined : t('workshopGeneration.run.noModel', { defaultValue: '请先选择模型' })}
            className={[
              'nodrag flex w-full box-border items-center justify-center gap-6px rounded-9px px-10px py-8px text-12px font-700 transition-all select-none',
              effectiveModel
                ? 'bg-[rgb(var(--primary-6))] text-white cursor-pointer hover:opacity-92 shadow-[0_4px_14px_rgba(var(--primary-6),0.32)]'
                : 'bg-[var(--color-fill-3)] text-[var(--color-text-3)] cursor-not-allowed',
            ].join(' ')}
          >
            {results.length > 0 ? (
              <Refresh theme='outline' size={13} strokeWidth={3} />
            ) : (
              <Play theme='outline' size={13} strokeWidth={3} />
            )}
            {runLabel}
          </div>
        )}
      </div>
    </div>
  );
};

export default React.memo(GeneratorCard) as React.FC<GeneratorCardProps>;
