/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * GroupNode — a translucent container that wraps its member nodes. It renders
 * behind the members (react-flow paints the parent first), drags them as a unit,
 * and connects to a generator card as an "input group" (its members feed the
 * card as ordered references). Removed only via its own menu — ungroup (keep
 * members) or delete-with-members — so the Delete key never orphans children.
 */

import React, { useEffect, useRef, useState } from 'react';
import { Handle, type NodeProps, Position, useStore } from '@xyflow/react';
import { DeleteFour, EditName, Ungroup } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useCanvasNode } from '../CanvasNodeContext';
import type { GroupFlowNode } from '../model';
import { KIND_META } from '../model';
import { HoverToolbar, ResizeFrame, ToolButton } from './nodeShared';

function GroupNodeImpl({ id, data, selected }: NodeProps<GroupFlowNode>) {
  const { t } = useTranslation();
  const api = useCanvasNode();
  const [hover, setHover] = useState(false);
  const [editing, setEditing] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const title = typeof data.title === 'string' && data.title ? data.title : t('workshopCanvas.node.group.defaultTitle', { defaultValue: '分组' });
  const memberCount = useStore((s) => {
    let n = 0;
    for (const node of s.nodeLookup.values()) if (node.parentId === id) n += 1;
    return n;
  });

  useEffect(() => {
    if (editing) inputRef.current?.focus();
  }, [editing]);

  const commitTitle = (value: string): void => {
    setEditing(false);
    const next = value.trim();
    if (next && next !== title) api.updateNodeData(id, { title: next });
  };

  return (
    <>
      <ResizeFrame visible={selected} minWidth={KIND_META.group.minWidth} minHeight={KIND_META.group.minHeight} />
      <div className='h-full w-full' onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}>
        <HoverToolbar show={hover || selected}>
          <ToolButton label={t('workshopCanvas.node.group.ungroup', { defaultValue: '解组（保留子节点）' })} onClick={() => api.ungroupNode(id)}>
            <Ungroup theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
          <ToolButton
            label={t('workshopCanvas.node.group.deleteWithChildren', { defaultValue: '删除组与内容' })}
            danger
            onClick={() => api.deleteGroupWithChildren(id)}
          >
            <DeleteFour theme='outline' size={15} strokeWidth={3} />
          </ToolButton>
        </HoverToolbar>

        {/* Handle mid-right so the group can feed a generator as an input group. */}
        <Handle
          type='source'
          position={Position.Right}
          isConnectable
          style={{ width: 11, height: 11, border: '2px solid var(--color-bg-2)', background: 'rgb(var(--primary-6))' }}
        />

        <div
          className={[
            'relative flex h-full w-full flex-col rounded-16px box-border border border-dashed transition-colors',
            selected ? 'border-[rgb(var(--primary-6))]' : 'border-[var(--color-border-3)]',
          ].join(' ')}
          style={{
            background: selected
              ? 'color-mix(in srgb, rgb(var(--primary-6)) 8%, transparent)'
              : 'color-mix(in srgb, var(--color-fill-2) 55%, transparent)',
            backdropFilter: 'blur(1px)',
          }}
        >
          {/* Title bar. */}
          <div className='flex shrink-0 items-center gap-7px rounded-t-15px px-11px py-7px' style={{ background: 'color-mix(in srgb, var(--color-fill-3) 60%, transparent)' }}>
            <span className='flex h-16px w-16px items-center justify-center rounded-4px text-[var(--color-text-3)]'>
              <Ungroup theme='outline' size={12} strokeWidth={3} />
            </span>
            {editing ? (
              <input
                ref={inputRef}
                defaultValue={title}
                onPointerDown={(e) => e.stopPropagation()}
                onKeyDown={(e) => {
                  e.stopPropagation();
                  if (e.key === 'Enter') commitTitle((e.target as HTMLInputElement).value);
                  else if (e.key === 'Escape') setEditing(false);
                }}
                onBlur={(e) => commitTitle(e.target.value)}
                className='nodrag min-w-0 flex-1 border-none bg-transparent text-12px font-700 text-[var(--color-text-1)] outline-none'
              />
            ) : (
              <span
                role='button'
                tabIndex={0}
                title={t('workshopCanvas.node.group.rename', { defaultValue: '重命名分组' })}
                onDoubleClick={(e) => {
                  e.stopPropagation();
                  setEditing(true);
                }}
                onClick={(e) => e.stopPropagation()}
                className='min-w-0 flex-1 truncate text-12px font-700 text-[var(--color-text-1)] cursor-text'
              >
                {title}
              </span>
            )}
            <span className='shrink-0 rounded-full bg-[var(--color-fill-2)] px-6px py-1px text-9px font-600 tabular-nums text-[var(--color-text-3)]'>
              {memberCount}
            </span>
            <span
              role='button'
              tabIndex={0}
              title={t('workshopCanvas.node.group.rename', { defaultValue: '重命名分组' })}
              onClick={(e) => {
                e.stopPropagation();
                setEditing(true);
              }}
              onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && setEditing(true)}
              className='grid h-18px w-18px shrink-0 place-items-center rounded-5px text-[var(--color-text-3)] cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]'
            >
              <EditName theme='outline' size={12} strokeWidth={3} />
            </span>
          </div>
          {/* Body is intentionally empty — member nodes render on top. */}
          <div className='min-h-0 flex-1' />
        </div>
      </div>
    </>
  );
}

export default React.memo(GroupNodeImpl);
