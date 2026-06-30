/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { Handle, Position, type Node, type NodeProps } from '@xyflow/react';
import { Crown } from '@icon-park/react';

/** Accent for the main node — the brand tone, so the lead/main agent reads as a
 * distinct structural role above the task DAG (mirrors the way TaskNode's kind
 * badges borrow `var(--brand)` for role, never a status color). Defined in every
 * theme preset. */
const MAIN_ACCENT = 'var(--brand)';

/** The data payload DagCanvas attaches to the synthetic main node. Kept minimal
 * (label + active highlight + click handler) so the node has zero coupling to
 * task wiring. */
export interface MainNodeData extends Record<string, unknown> {
  /** Localized "main · 主 agent" label (computed in DagCanvas so the node stays
   * free of i18n wiring; formal i18n key lands in F10). */
  label: string;
  /** Whether the lead/main agent conversation is the currently-projected view —
   * highlights the node (the projected task, if any, is `null`). */
  active?: boolean;
  /** Click handler — returns the content area to the main agent conversation. */
  onOpen: () => void;
}

/** Strongly-typed node alias so NodeProps narrows `data` for us. */
export type MainFlowNode = Node<MainNodeData, 'main'>;

/**
 * MainNode — the synthetic lead/main agent node rendered ABOVE the in-degree-0
 * root tasks when DagCanvas is given an `onOpenMain` callback. Styled to match
 * {@link TaskNode}'s on-brand card (theme variables only, no hardcoded hex): a
 * crown glyph + the localized label, a brand left-border, and an `active`
 * highlight ring when the main conversation is the projected view. The whole
 * card is a `role="button"` that calls `data.onOpen()` to return to the main
 * agent. A single downward `source` handle anchors the edges to the root tasks.
 */
function MainNodeImpl({ data, selected }: NodeProps<MainFlowNode>) {
  const active = data.active ?? false;
  // Selection always wins; otherwise an active main node gets a soft brand ring
  // layered under the base drop shadow so "you are here" reads clearly without
  // fighting the brand left-border.
  const baseShadow = selected
    ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 22%, transparent), 0 6px 18px rgba(0,0,0,0.14)'
    : active
      ? `0 0 0 2px color-mix(in srgb, ${MAIN_ACCENT} 42%, transparent), 0 2px 10px rgba(0,0,0,0.10)`
      : '0 2px 10px rgba(0,0,0,0.10)';

  return (
    <div
      role='button'
      tabIndex={0}
      aria-label={data.label}
      onClick={data.onOpen}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          data.onOpen();
        }
      }}
      className='nomi-dag-node group flex w-220px cursor-pointer select-none flex-col gap-8px rd-12px px-14px py-12px transition-all duration-150 outline-none'
      style={{
        background: 'var(--bg-2)',
        border: `1px solid ${selected ? 'rgb(var(--primary-6))' : 'var(--border-base)'}`,
        borderLeft: `3px solid ${MAIN_ACCENT}`,
        boxShadow: baseShadow,
      }}
    >
      {/* Title row: crown badge + main label */}
      <div className='flex items-center gap-8px'>
        <span
          className='flex size-22px shrink-0 items-center justify-center rd-8px'
          style={{
            color: MAIN_ACCENT,
            background: `color-mix(in srgb, ${MAIN_ACCENT} 14%, transparent)`,
            border: `1px solid color-mix(in srgb, ${MAIN_ACCENT} 32%, transparent)`,
          }}
        >
          <Crown theme='outline' size='13' strokeWidth={4} className='line-height-0' />
        </span>
        <span className='min-w-0 flex-1 text-13px font-600 leading-18px text-t-primary line-clamp-2'>
          {data.label}
        </span>
      </div>

      {/* Outgoing anchor (bottom) → connects down to each root task. */}
      <Handle
        type='source'
        position={Position.Bottom}
        isConnectable={false}
        style={{ width: 7, height: 7, background: 'var(--bg-5)', border: 'none' }}
      />
    </div>
  );
}

export default React.memo(MainNodeImpl);
