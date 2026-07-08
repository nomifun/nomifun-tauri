/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Input summary strip: reactive counts of upstream (edge-connected) inputs by
 * kind, plus removable chips for each `@`-mention the card carries. Reads the
 * card's incoming connections + source node data reactively so the counts track
 * the graph live.
 */

import React, { useMemo } from 'react';
import { useNodeConnections, useNodesData, useReactFlow } from '@xyflow/react';
import { useTranslation } from 'react-i18next';
import { CloseSmall, FileText, Link, Pic, VideoTwo } from '@icon-park/react';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import type { WorkshopAssetKind } from '../types';
import { collectNodeCandidates, parseMentionRef } from './pipeline';

export interface InputSummaryProps {
  selfId: string;
  mentions: string[];
  onRemoveMention: (ref: string) => void;
}

const KIND_ICON: Record<WorkshopAssetKind, React.ReactNode> = {
  image: <Pic theme='outline' size={12} strokeWidth={3} />,
  video: <VideoTwo theme='outline' size={12} strokeWidth={3} />,
  text: <FileText theme='outline' size={12} strokeWidth={3} />,
};

/** Map a source node's { type, data } to the asset kind it contributes. */
function sourceKind(type: string | undefined, data: Record<string, unknown> | undefined): WorkshopAssetKind | null {
  if (type === 'image') return typeof data?.assetId === 'string' ? 'image' : null;
  if (type === 'video') return typeof data?.assetId === 'string' ? 'video' : null;
  if (type === 'text') return typeof data?.content === 'string' && data.content.trim() ? 'text' : null;
  if (type === 'generator') {
    const results = Array.isArray(data?.resultAssetIds) ? (data.resultAssetIds as string[]) : [];
    if (!results.length) return null;
    const mode = typeof data?.mode === 'string' ? data.mode : 'image';
    return mode === 'video' ? 'video' : mode === 'text' ? 'text' : 'image';
  }
  return null;
}

const InputSummary: React.FC<InputSummaryProps> = ({ selfId, mentions, onRemoveMention }) => {
  const { t } = useTranslation();
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();
  const connections = useNodeConnections({ id: selfId, handleType: 'target' });
  const sourceIds = useMemo(() => [...new Set(connections.map((c) => c.source))], [connections]);
  const sources = useNodesData(sourceIds);

  const counts = useMemo(() => {
    const c: Record<WorkshopAssetKind, number> = { image: 0, video: 0, text: 0 };
    for (const s of sources) {
      const kind = sourceKind(s?.type, s?.data as Record<string, unknown> | undefined);
      if (kind) c[kind] += 1;
    }
    return c;
  }, [sources]);

  const mentionChips = useMemo(() => {
    const labelMap = new Map<string, string>();
    for (const cand of collectNodeCandidates(rf.getNodes(), selfId)) labelMap.set(cand.ref, cand.label);
    return mentions.map((ref) => {
      const parsed = parseMentionRef(ref);
      const kind = parsed?.kind ?? 'image';
      const label =
        labelMap.get(ref) ??
        (parsed?.source === 'asset'
          ? t('workshopGeneration.mention.assetLabel', { defaultValue: '资产库素材' })
          : t('workshopGeneration.mention.missing', { defaultValue: '已失效' }));
      return { ref, kind, label };
    });
  }, [mentions, rf, selfId, t]);

  const hasCounts = counts.image + counts.video + counts.text > 0;
  if (!hasCounts && mentionChips.length === 0) return null;

  const countChips: { kind: WorkshopAssetKind; n: number }[] = (['image', 'text', 'video'] as WorkshopAssetKind[])
    .filter((k) => counts[k] > 0)
    .map((k) => ({ kind: k, n: counts[k] }));

  return (
    <div className='flex flex-wrap items-center gap-5px'>
      {countChips.map(({ kind, n }) => (
        <span
          key={kind}
          className='inline-flex items-center gap-4px rounded-full bg-[var(--color-fill-2)] px-7px py-2px text-10px font-600 text-[var(--color-text-2)]'
          title={t('workshopGeneration.input.upstream', { defaultValue: '上游连线' })}
        >
          <Link theme='outline' size={10} strokeWidth={3} className='text-[var(--color-text-3)]' />
          {KIND_ICON[kind]}
          <span className='tabular-nums'>{n}</span>
        </span>
      ))}
      {mentionChips.map((chip) => (
        <span
          key={chip.ref}
          className='inline-flex items-center gap-4px rounded-full border border-solid border-[rgba(var(--primary-6),0.3)] bg-[rgba(var(--primary-6),0.08)] px-7px py-2px text-10px font-600 text-[rgb(var(--primary-6))]'
        >
          {KIND_ICON[chip.kind]}
          <span className='max-w-88px truncate'>{chip.label}</span>
          <span
            role='button'
            tabIndex={0}
            title={t('workshopGeneration.mention.remove', { defaultValue: '移除引用' })}
            onClick={(e) => {
              e.stopPropagation();
              onRemoveMention(chip.ref);
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onRemoveMention(chip.ref);
              }
            }}
            className='nodrag flex cursor-pointer items-center opacity-70 hover:opacity-100'
          >
            <CloseSmall theme='outline' size={11} strokeWidth={3} />
          </span>
        </span>
      ))}
    </div>
  );
};

export default InputSummary;
