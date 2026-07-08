/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CompareNode — an A/B wipe. It takes exactly two upstream image sources (image
 * nodes or generator cards with a result) and overlays them in one box; dragging
 * the divider wipes between A (left) and B (right). Fewer than two ready inputs
 * shows a guidance state.
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { type NodeProps, useNodesData, useStore } from '@xyflow/react';
import { Contrast, DeleteFour } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import { useWorkshopMedia } from '../media';
import type { CompareFlowNode } from '../model';
import { KIND_META } from '../model';
import { upstreamPrimary } from './upstream';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';

const clamp01 = (v: number): number => Math.min(1, Math.max(0, v));

const MediaLayer: React.FC<{ assetId: string | null; kind: 'image' | 'video' | undefined }> = ({ assetId, kind }) => {
  const media = useWorkshopMedia(kind === 'image' || kind === 'video' ? assetId : null);
  if (media.status !== 'ready') {
    return (
      <div className='flex h-full w-full items-center justify-center'>
        <span className='h-16px w-16px animate-spin rounded-full border-2 border-solid border-[var(--color-fill-3)] border-t-[rgb(var(--primary-6))]' />
      </div>
    );
  }
  return kind === 'video' ? (
    <video src={media.url} muted loop autoPlay playsInline className='h-full w-full bg-black object-contain' />
  ) : (
    <img src={media.url} alt='' draggable={false} className='h-full w-full select-none object-contain' />
  );
};

function CompareNodeImpl({ id, data, selected }: NodeProps<CompareFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);
  const boxRef = useRef<HTMLDivElement | null>(null);
  const [split, setSplit] = useState<number>(() => clamp01(typeof data.split === 'number' ? data.split : 0.5));

  // Up to two image-bearing upstream sources, in edge order (string key = stable).
  const sourceKey = useStore((s) => {
    const ids: string[] = [];
    for (const e of s.edges) {
      if (e.target !== id) continue;
      const n = s.nodeLookup.get(e.source);
      if (!n) continue;
      if ((n.type === 'image' || n.type === 'video' || n.type === 'generator') && !ids.includes(e.source)) ids.push(e.source);
      if (ids.length >= 2) break;
    }
    return ids.join('|');
  });
  const [idA, idB] = useMemo(() => (sourceKey ? sourceKey.split('|') : []), [sourceKey]);
  const dataA = useNodesData(idA ?? '');
  const dataB = useNodesData(idB ?? '');

  const contribA = useMemo(() => upstreamPrimary(dataA), [dataA]);
  const contribB = useMemo(() => upstreamPrimary(dataB), [dataB]);
  const kindA = contribA?.kind === 'video' ? 'video' : 'image';
  const kindB = contribB?.kind === 'video' ? 'video' : 'image';
  const ready = !!contribA?.assetId && !!contribB?.assetId;

  const dragRef = useRef(false);

  // Re-sync the local divider from the persisted `data.split` when it changes
  // out-of-band (undo / redo / agent or collab update). Gated on `dragRef` so an
  // active drag isn't interrupted, and threshold-compared to avoid feedback from
  // the toFixed(3) round-trip we persist on drag end.
  useEffect(() => {
    if (dragRef.current) return;
    const v = clamp01(typeof data.split === 'number' ? data.split : 0.5);
    setSplit((prev) => (Math.abs(prev - v) > 1e-4 ? v : prev));
  }, [data.split]);

  const updateSplitFromClientX = useCallback((clientX: number) => {
    const rect = boxRef.current?.getBoundingClientRect();
    if (!rect || rect.width === 0) return;
    setSplit(clamp01((clientX - rect.left) / rect.width));
  }, []);
  const onPointerDown = useCallback(
    (e: React.PointerEvent) => {
      e.stopPropagation();
      dragRef.current = true;
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      updateSplitFromClientX(e.clientX);
    },
    [updateSplitFromClientX]
  );
  const onPointerMove = useCallback(
    (e: React.PointerEvent) => {
      if (!dragRef.current) return;
      updateSplitFromClientX(e.clientX);
    },
    [updateSplitFromClientX]
  );
  const onPointerUp = useCallback(() => {
    if (!dragRef.current) return;
    dragRef.current = false;
    // Persist the divider once per drag (single history entry).
    api.updateNodeData(id, { split: Number(split.toFixed(3)) });
  }, [api, id, split]);

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.compare.minWidth} minHeight={KIND_META.compare.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected}>
          <NodeHandles sides='target' />
          <div className='flex items-center gap-6px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-10px py-6px'>
            <span
              className='flex h-18px w-18px items-center justify-center rounded-5px text-[rgb(var(--primary-6))]'
              style={{ background: 'rgba(var(--primary-6),0.12)' }}
            >
              <Contrast theme='outline' size={11} strokeWidth={3} />
            </span>
            <span className='text-11px font-700 text-[var(--color-text-1)]'>
              {t('workshopCanvas.node.compare.title', { defaultValue: '对比' })}
            </span>
          </div>

          {ready ? (
            <div
              ref={boxRef}
              className='nodrag nowheel relative min-h-0 flex-1 select-none overflow-hidden'
              style={{ background: 'var(--color-fill-1)' }}
            >
              {/* Base layer = B (right). */}
              <div className='absolute inset-0'>
                <MediaLayer assetId={contribB?.assetId ?? null} kind={kindB} />
              </div>
              {/* Overlay = A (left), clipped to the divider. */}
              <div className='absolute inset-0' style={{ clipPath: `inset(0 ${(1 - split) * 100}% 0 0)` }}>
                <MediaLayer assetId={contribA?.assetId ?? null} kind={kindA} />
              </div>

              {/* Labels. */}
              <span className='absolute left-6px top-6px rounded-full bg-black/55 px-6px py-1px text-9px font-700 text-white backdrop-blur-sm'>A</span>
              <span className='absolute right-6px top-6px rounded-full bg-black/55 px-6px py-1px text-9px font-700 text-white backdrop-blur-sm'>B</span>

              {/* Divider + draggable handle. */}
              <div
                role='slider'
                aria-label={t('workshopCanvas.node.compare.divider', { defaultValue: '对比分界线' })}
                aria-valuenow={Math.round(split * 100)}
                aria-valuemin={0}
                aria-valuemax={100}
                tabIndex={0}
                onPointerDown={onPointerDown}
                onPointerMove={onPointerMove}
                onPointerUp={onPointerUp}
                onKeyDown={(e) => {
                  if (e.key === 'ArrowLeft') setSplit((s) => clamp01(s - 0.02));
                  else if (e.key === 'ArrowRight') setSplit((s) => clamp01(s + 0.02));
                }}
                className='absolute top-0 bottom-0 z-10 flex w-16px -translate-x-1/2 cursor-ew-resize items-center justify-center'
                style={{ left: `${split * 100}%` }}
              >
                <span className='pointer-events-none absolute top-0 bottom-0 w-2px bg-white shadow-[0_0_0_1px_rgba(0,0,0,0.25)]' />
                <span className='pointer-events-none grid h-22px w-22px place-items-center rounded-full bg-white text-[#333] shadow-[0_2px_8px_rgba(0,0,0,0.35)]'>
                  <Contrast theme='outline' size={13} strokeWidth={3} />
                </span>
              </div>
            </div>
          ) : (
            <div className='flex min-h-0 flex-1 flex-col items-center justify-center gap-6px px-14px text-center text-[var(--color-text-3)]'>
              <Contrast theme='outline' size={20} strokeWidth={3} />
              <span className='text-11px leading-[1.5]'>
                {t('workshopCanvas.node.compare.needTwo', { defaultValue: '连入两个图源以对比（当前 {{n}}/2）', n: [contribA?.assetId, contribB?.assetId].filter(Boolean).length })}
              </span>
            </div>
          )}
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(CompareNodeImpl);
