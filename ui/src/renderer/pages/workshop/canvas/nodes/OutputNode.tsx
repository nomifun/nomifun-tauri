/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * OutputNode — a mid-chain inspector. It mirrors, in real time, the current
 * primary result of the single node wired into it (a generator card's main
 * output, or an image / video node's asset), so a long chain can be checked
 * without opening each card. Double-click opens the shared big-image preview.
 */

import React, { useMemo, useState } from 'react';
import { type NodeProps, useNodesData, useStore } from '@xyflow/react';
import { DeleteFour, Info, Left, PreviewOpen } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import { useWorkshopMedia } from '../media';
import type { OutputFlowNode } from '../model';
import { KIND_META } from '../model';
import { upstreamPrimary } from './upstream';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';

function OutputNodeImpl({ id, selected }: NodeProps<OutputFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);

  // The single upstream source wired into this inspector (first incoming edge).
  const sourceId = useStore((s) => {
    for (const e of s.edges) if (e.target === id) return e.source;
    return null;
  });
  const upstream = useNodesData(sourceId ?? '');

  const resolved = useMemo(() => upstreamPrimary(upstream), [upstream]);

  const isMedia = resolved?.kind === 'image' || resolved?.kind === 'video';
  const media = useWorkshopMedia(isMedia ? resolved?.assetId ?? null : null);
  const assetId = resolved?.assetId ?? null;

  const openPreview = (): void => {
    if (resolved?.kind === 'image' && assetId) api.openImagePreview([assetId], 0);
  };

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.output.minWidth} minHeight={KIND_META.output.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          {resolved?.kind === 'image' && (
            <ToolButton label={t('workshopCanvas.node.output.preview', { defaultValue: '大图预览' })} onClick={openPreview}>
              <PreviewOpen theme='outline' size={15} strokeWidth={3} />
            </ToolButton>
          )}
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected} onDoubleClick={openPreview}>
          <NodeHandles sides='target' />
          <div className='flex items-center gap-6px border-b border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-t-0 px-10px py-6px'>
            <span
              className='flex h-18px w-18px items-center justify-center rounded-5px text-[var(--color-text-3)]'
              style={{ background: 'var(--color-fill-2)' }}
            >
              <PreviewOpen theme='outline' size={11} strokeWidth={3} />
            </span>
            <span className='text-11px font-700 text-[var(--color-text-1)]'>
              {t('workshopCanvas.node.output.title', { defaultValue: '输出检视' })}
            </span>
          </div>

          <div className='relative min-h-0 flex-1' style={{ background: 'var(--color-fill-1)' }}>
            {!sourceId || !resolved ? (
              <div className='flex h-full w-full flex-col items-center justify-center gap-6px px-14px text-center text-[var(--color-text-3)]'>
                <Left theme='outline' size={18} strokeWidth={3} />
                <span className='text-11px leading-[1.5]'>
                  {t('workshopCanvas.node.output.empty', { defaultValue: '从左侧连入一个节点以检视其当前结果' })}
                </span>
              </div>
            ) : resolved.kind === 'text' ? (
              <div className='nowheel h-full w-full overflow-y-auto whitespace-pre-wrap break-words px-10px py-8px text-11px leading-[1.55] text-[var(--color-text-1)]'>
                {resolved.text ?? (
                  <span className='text-[var(--color-text-3)]'>{t('workshopCanvas.node.output.textUpstream', { defaultValue: '（上游文本节点）' })}</span>
                )}
              </div>
            ) : media.status === 'ready' ? (
              resolved.kind === 'video' ? (
                <video src={media.url} controls playsInline className='nodrag h-full w-full bg-black object-contain' />
              ) : (
                <img src={media.url} alt='' draggable={false} className='h-full w-full select-none object-contain' />
              )
            ) : media.status === 'error' ? (
              <div className='flex h-full w-full flex-col items-center justify-center gap-6px text-[rgb(var(--danger-6))]'>
                <Info theme='outline' size={18} strokeWidth={3} />
                <span className='text-11px'>{t('workshopCanvas.node.output.loadFailed', { defaultValue: '加载失败' })}</span>
              </div>
            ) : (
              <div className='flex h-full w-full items-center justify-center'>
                <span className='h-18px w-18px animate-spin rounded-full border-2 border-solid border-[var(--color-fill-3)] border-t-[rgb(var(--primary-6))]' />
              </div>
            )}
          </div>
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(OutputNodeImpl);
