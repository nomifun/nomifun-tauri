/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * GeneratorNode — the canvas host for the M7 generation card.
 *
 * This wrapper draws only the react-flow node chrome (resize frame, connection
 * handles, hover toolbar, card shell); all generation behaviour — mode / model
 * pickers, prompt + `@`-mentions, params, run/cancel, results, and the
 * continuous-edit chain — lives in {@link GeneratorCard} under
 * `pages/workshop/generation/`, driven entirely through the serialisable node
 * `data` via the canvas `updateNodeData`.
 */

import React, { useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { CopyOne, DeleteFour } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import type { GeneratorFlowNode } from '../model';
import { KIND_META } from '../model';
import { GeneratorCard } from '../../generation';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';

function GeneratorNodeImpl({ id, data, selected }: NodeProps<GeneratorFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.generator.minWidth} minHeight={KIND_META.generator.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          <ToolButton label={t('workshopCanvas.node.duplicate', { defaultValue: '复制副本' })} onClick={() => api.duplicateNode(id)}>
            <CopyOne theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected}>
          <NodeHandles />
          <GeneratorCard id={id} data={data} />
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(GeneratorNodeImpl);
