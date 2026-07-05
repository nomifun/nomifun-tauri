/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * PlaceholderNode — the fallback card for node kinds whose interactions land in
 * M8 (`loop` / `compare` / `output` / `group`). Renders a labelled "coming
 * soon" surface; no interaction beyond select / move / delete.
 */

import React, { useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { Components, DeleteFour, Info } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import type { PlaceholderFlowNode } from '../model';
import { KIND_META } from '../model';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';

function PlaceholderNodeImpl({ id, type, selected }: NodeProps<PlaceholderFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);
  const kind = type;
  const meta = KIND_META[kind] ?? KIND_META.group;
  const kindLabel = t(`workshopCanvas.node.kind.${kind}`, { defaultValue: kind });

  return (
    <>
      <ResizeFrame visible={selected} minWidth={meta.minWidth} minHeight={meta.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected}>
          <NodeHandles />
          <div className='flex h-full w-full flex-col items-center justify-center gap-8px px-14px text-center'>
            <span
              className='flex h-36px w-36px items-center justify-center rounded-10px text-[var(--color-text-3)]'
              style={{ background: 'var(--color-fill-2)' }}
            >
              <Components theme='outline' size={18} strokeWidth={3} />
            </span>
            <span className='text-13px font-700 text-[var(--color-text-1)]'>{kindLabel}</span>
            <span className='inline-flex items-center gap-4px rounded-full bg-[var(--color-fill-2)] px-8px py-3px text-10px text-[var(--color-text-3)]'>
              <Info theme='outline' size={11} strokeWidth={3} />
              {t('workshopCanvas.node.comingSoon', { defaultValue: '即将上线' })}
            </span>
          </div>
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(PlaceholderNodeImpl);
