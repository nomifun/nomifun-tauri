/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect, useRef, useState } from 'react';
import type { NodeProps } from '@xyflow/react';
import { DeleteFour, Minus, Plus, Text } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import type { TextFlowNode } from '../model';
import { HoverToolbar, NodeCard, NodeHandles, ResizeFrame, ToolButton } from './nodeShared';
import { KIND_META } from '../model';

const MIN_FONT = 11;
const MAX_FONT = 40;

function TextNodeImpl({ id, data, selected }: NodeProps<TextFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(data.content ?? '');
  const [hover, setHover] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  const fontSize = typeof data.fontSize === 'number' ? data.fontSize : 14;

  useEffect(() => {
    if (!editing) setDraft(data.content ?? '');
  }, [data.content, editing]);

  useEffect(() => {
    if (editing) {
      const el = textareaRef.current;
      if (el) {
        el.focus();
        el.setSelectionRange(el.value.length, el.value.length);
      }
    }
  }, [editing]);

  const commit = (): void => {
    setEditing(false);
    if (draft !== (data.content ?? '')) api.updateNodeData(id, { content: draft });
  };

  const changeFont = (delta: number): void => {
    const next = Math.max(MIN_FONT, Math.min(MAX_FONT, fontSize + delta));
    if (next !== fontSize) api.updateNodeData(id, { fontSize: next });
  };

  const showTools = hover || selected;
  const empty = !(data.content ?? '').trim();

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.text.minWidth} minHeight={KIND_META.text.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={showTools}>
          <ToolButton label={t('workshopCanvas.node.text.smaller', { defaultValue: '缩小字号' })} onClick={() => changeFont(-2)}>
            <Minus theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <span className='min-w-24px px-2px text-center text-11px font-600 tabular-nums text-[var(--color-text-3)]'>{fontSize}</span>
          <ToolButton label={t('workshopCanvas.node.text.larger', { defaultValue: '放大字号' })} onClick={() => changeFont(2)}>
            <Plus theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <span className='mx-2px h-14px w-1px bg-[var(--color-border-2)]' />
          <ToolButton label={t('workshopCanvas.node.delete', { defaultValue: '删除' })} danger onClick={() => api.removeNode(id)}>
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        <NodeCard selected={selected} onDoubleClick={() => setEditing(true)}>
          <NodeHandles />
          {editing ? (
            <textarea
              ref={textareaRef}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onBlur={commit}
              onKeyDown={(e) => {
                if (e.key === 'Escape') {
                  e.preventDefault();
                  setDraft(data.content ?? '');
                  setEditing(false);
                } else if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  commit();
                }
                e.stopPropagation();
              }}
              placeholder={t('workshopCanvas.node.text.placeholder', { defaultValue: '输入文本…' })}
              className='nodrag nowheel h-full w-full resize-none border-none bg-transparent p-12px leading-[1.5] text-[var(--color-text-1)] outline-none placeholder:text-[var(--color-text-3)]'
              style={{ fontSize }}
            />
          ) : (
            <div
              className='h-full w-full overflow-hidden whitespace-pre-wrap break-words p-12px leading-[1.5]'
              style={{ fontSize, color: empty ? 'var(--color-text-3)' : 'var(--color-text-1)' }}
            >
              {empty ? (
                <span className='flex h-full w-full flex-col items-center justify-center gap-6px text-center'>
                  <Text theme='outline' size={20} strokeWidth={3} />
                  <span className='text-12px'>{t('workshopCanvas.node.text.empty', { defaultValue: '双击编辑文本' })}</span>
                </span>
              ) : (
                data.content
              )}
            </div>
          )}
        </NodeCard>
      </div>
    </>
  );
}

export default React.memo(TextNodeImpl);
