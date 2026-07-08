/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Shared chrome for workshop canvas nodes: the rounded card shell, the
 * left-in / right-out connection handles, the corner resize frame, and the
 * hover toolbar primitives. Theme variables + UnoCSS only.
 */

import React from 'react';
import { Handle, NodeResizer, Position } from '@xyflow/react';
import { useCanvasNode } from '../CanvasNodeContext';

// ─── Connection handles (left = input, right = output) ────────────────────────

const HANDLE_STYLE: React.CSSProperties = {
  width: 11,
  height: 11,
  border: '2px solid var(--color-bg-2)',
  background: 'rgb(var(--primary-6))',
};

export const NodeHandles: React.FC<{ connectable?: boolean; sides?: 'both' | 'target' | 'source' }> = ({
  connectable = true,
  sides = 'both',
}) => (
  <>
    {sides !== 'source' && <Handle type='target' position={Position.Left} isConnectable={connectable} style={HANDLE_STYLE} />}
    {sides !== 'target' && <Handle type='source' position={Position.Right} isConnectable={connectable} style={HANDLE_STYLE} />}
  </>
);

// ─── Corner resize frame ──────────────────────────────────────────────────────

export const ResizeFrame: React.FC<{
  visible: boolean;
  minWidth: number;
  minHeight: number;
  keepAspectRatio?: boolean;
}> = ({ visible, minWidth, minHeight, keepAspectRatio }) => {
  const api = useCanvasNode();
  return (
    <NodeResizer
      isVisible={visible}
      minWidth={minWidth}
      minHeight={minHeight}
      keepAspectRatio={keepAspectRatio}
      color='rgb(var(--primary-6))'
      handleStyle={{ width: 9, height: 9, borderRadius: 3 }}
      // Hide the resizer's edge lines: every node draws its own rounded selected
      // border + ring (NodeCard / GroupNode). A visible resizer line is a
      // sharp-cornered rectangle at the node bounds, so it doubles that border
      // and overshoots the card's rounded corners. NodeResizer forces
      // `borderColor: color` after our lineStyle, so `borderColor: transparent`
      // wouldn't stick — zero the border width instead. The 1px line div stays
      // as an edge-drag hit area; corner handles remain the resize affordance.
      lineStyle={{ borderWidth: 0 }}
      onResizeStart={() => api.beginInteraction()}
      onResizeEnd={() => api.commitInteraction()}
    />
  );
};

// ─── Hover toolbar ────────────────────────────────────────────────────────────

/** A floating action bar shown above the node on hover / when selected. */
export const HoverToolbar: React.FC<{ children: React.ReactNode; show: boolean }> = ({ children, show }) => (
  <div
    className={[
      'nowheel absolute -top-11px left-1/2 z-10 flex -translate-x-1/2 items-center gap-2px',
      'rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-3px py-3px',
      'shadow-[0_6px_20px_rgba(0,0,0,0.18)] backdrop-blur-md transition-all duration-120',
      show ? 'pointer-events-auto opacity-100' : 'pointer-events-none opacity-0 translate-y-2px',
    ].join(' ')}
    onDoubleClick={(e) => e.stopPropagation()}
  >
    {children}
  </div>
);

export const ToolButton: React.FC<{
  label: string;
  onClick: () => void;
  danger?: boolean;
  children: React.ReactNode;
}> = ({ label, onClick, danger, children }) => (
  <div
    role='button'
    tabIndex={0}
    title={label}
    aria-label={label}
    onClick={(e) => {
      e.stopPropagation();
      onClick();
    }}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        e.stopPropagation();
        onClick();
      }
    }}
    className={[
      'grid h-24px w-24px place-items-center rounded-6px cursor-pointer transition-colors',
      danger
        ? 'text-[var(--color-text-3)] hover:!bg-[rgba(var(--danger-6),0.12)] hover:!text-[rgb(var(--danger-6))]'
        : 'text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
    ].join(' ')}
  >
    {children}
  </div>
);

// ─── Card shell ───────────────────────────────────────────────────────────────

/** The rounded, shadowed node surface with the selection ring. */
export const NodeCard: React.FC<{
  selected: boolean;
  children: React.ReactNode;
  className?: string;
  onDoubleClick?: (e: React.MouseEvent) => void;
}> = ({ selected, children, className, onDoubleClick }) => (
  <div
    onDoubleClick={onDoubleClick}
    className={[
      'relative flex h-full w-full flex-col overflow-hidden rounded-13px box-border',
      'border border-solid bg-[var(--color-bg-2)] transition-shadow duration-120',
      selected ? 'border-[rgb(var(--primary-6))]' : 'border-[var(--color-border-2)]',
      className ?? '',
    ].join(' ')}
    style={{
      boxShadow: selected
        ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 22%, transparent), 0 8px 24px rgba(0,0,0,0.16)'
        : '0 4px 16px rgba(0,0,0,0.10)',
    }}
  >
    {children}
  </div>
);

/** Empty-state upload dropzone shared by image / video nodes. */
export const UploadPlaceholder: React.FC<{
  icon: React.ReactNode;
  label: string;
  hint: string;
  onClick: () => void;
}> = ({ icon, label, hint, onClick }) => (
  <div
    role='button'
    tabIndex={0}
    onClick={(e) => {
      e.stopPropagation();
      onClick();
    }}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        e.stopPropagation();
        onClick();
      }
    }}
    className={[
      'flex h-full w-full flex-col items-center justify-center gap-8px px-14px text-center cursor-pointer select-none',
      'text-[var(--color-text-3)] transition-colors hover:text-[rgb(var(--primary-6))]',
    ].join(' ')}
  >
    <span
      className='flex h-40px w-40px items-center justify-center rounded-12px'
      style={{
        background: 'linear-gradient(150deg, rgba(var(--primary-5),0.14) 0%, rgba(var(--primary-6),0.26) 100%)',
        border: '1px solid rgba(var(--primary-6),0.2)',
        color: 'rgb(var(--primary-6))',
      }}
    >
      {icon}
    </span>
    <span className='text-13px font-600 text-[var(--color-text-2)]'>{label}</span>
    <span className='text-11px leading-[1.5]'>{hint}</span>
  </div>
);
