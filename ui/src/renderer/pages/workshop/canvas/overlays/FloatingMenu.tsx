/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * FloatingMenu — the shared popup used by the pane / node / edge context menus
 * and the "drop-a-connection-into-empty-space" quick-create menu. Positioned in
 * canvas-wrapper coordinates, clamped on-screen, and dismissed on outside click,
 * scroll, or Escape.
 */

import React, { useEffect, useLayoutEffect, useRef, useState } from 'react';

export type MenuEntry =
  | { type: 'item'; key: string; label: string; icon?: React.ReactNode; danger?: boolean; disabled?: boolean; onClick: () => void }
  | { type: 'divider'; key: string }
  | { type: 'header'; key: string; label: string };

export interface FloatingMenuProps {
  /** Position in canvas-wrapper (offset-parent) pixels. */
  x: number;
  y: number;
  entries: MenuEntry[];
  onClose: () => void;
}

const FloatingMenu: React.FC<FloatingMenuProps> = ({ x, y, entries, onClose }) => {
  const ref = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState({ x, y });

  // Clamp within the offset parent so the menu never spills off-canvas.
  useLayoutEffect(() => {
    const el = ref.current;
    const parent = el?.offsetParent as HTMLElement | null;
    if (!el || !parent) return;
    const { width, height } = el.getBoundingClientRect();
    const maxX = parent.clientWidth - width - 8;
    const maxY = parent.clientHeight - height - 8;
    setPos({ x: Math.max(8, Math.min(x, maxX)), y: Math.max(8, Math.min(y, maxY)) });
  }, [x, y, entries.length]);

  useEffect(() => {
    const onDown = (e: MouseEvent): void => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') onClose();
    };
    // Capture so we beat react-flow's own pane handlers.
    window.addEventListener('mousedown', onDown, true);
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('keydown', onKey);
    };
  }, [onClose]);

  return (
    <div
      ref={ref}
      className='absolute z-30 min-w-160px overflow-hidden rounded-11px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] py-4px shadow-[0_12px_36px_rgba(0,0,0,0.24)]'
      style={{ left: pos.x, top: pos.y, backdropFilter: 'blur(8px)' }}
      onContextMenu={(e) => e.preventDefault()}
    >
      {entries.map((entry) => {
        if (entry.type === 'divider') return <div key={entry.key} className='my-4px h-1px bg-[var(--color-border-2)]' />;
        if (entry.type === 'header')
          return (
            <div key={entry.key} className='px-12px pb-3px pt-4px text-10px font-600 uppercase tracking-wide text-[var(--color-text-3)]'>
              {entry.label}
            </div>
          );
        return (
          <div
            key={entry.key}
            role='button'
            tabIndex={entry.disabled ? -1 : 0}
            aria-disabled={entry.disabled}
            onClick={() => {
              if (entry.disabled) return;
              entry.onClick();
              onClose();
            }}
            onKeyDown={(e) => {
              if (entry.disabled) return;
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                entry.onClick();
                onClose();
              }
            }}
            className={[
              'mx-4px flex items-center gap-9px rounded-7px px-9px py-6px text-13px transition-colors',
              entry.disabled
                ? 'cursor-not-allowed text-[var(--color-text-4,var(--color-text-3))] opacity-60'
                : entry.danger
                  ? 'cursor-pointer text-[var(--color-text-2)] hover:bg-[rgba(var(--danger-6),0.1)] hover:text-[rgb(var(--danger-6))]'
                  : 'cursor-pointer text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
            ].join(' ')}
          >
            {entry.icon && <span className='flex h-15px w-15px shrink-0 items-center justify-center'>{entry.icon}</span>}
            <span className='truncate'>{entry.label}</span>
          </div>
        );
      })}
    </div>
  );
};

export default FloatingMenu;
