/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * A lightweight anchored popover rendered into a body portal. Because it lives
 * outside the react-flow transform, it never gets clipped by the node or panned
 * with the canvas; it positions itself against the trigger's screen rect and
 * flips above when there isn't room below. Closes on outside pointer-down,
 * Escape, or any scroll/resize (the anchor rect would otherwise go stale).
 */

import React, { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

export interface FloatingProps {
  anchorRect: DOMRect | null;
  open: boolean;
  onClose: () => void;
  /** Fixed width; defaults to the anchor's width. */
  width?: number;
  maxHeight?: number;
  children: React.ReactNode;
}

const MARGIN = 6;
const VIEWPORT_PAD = 10;

const Floating: React.FC<FloatingProps> = ({ anchorRect, open, onClose, width, maxHeight = 320, children }) => {
  const ref = useRef<HTMLDivElement | null>(null);
  const [style, setStyle] = useState<React.CSSProperties>({ visibility: 'hidden' });

  useLayoutEffect(() => {
    if (!open || !anchorRect) return;
    const el = ref.current;
    const h = el?.offsetHeight ?? maxHeight;
    const w = width ?? anchorRect.width;
    const spaceBelow = window.innerHeight - anchorRect.bottom;
    const above = spaceBelow < h + MARGIN + VIEWPORT_PAD && anchorRect.top > spaceBelow;
    const top = above ? Math.max(VIEWPORT_PAD, anchorRect.top - h - MARGIN) : anchorRect.bottom + MARGIN;
    const left = Math.min(Math.max(VIEWPORT_PAD, anchorRect.left), window.innerWidth - w - VIEWPORT_PAD);
    setStyle({ position: 'fixed', top, left, width: w, zIndex: 2200, visibility: 'visible' });
  }, [open, anchorRect, width, maxHeight]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent): void => {
      if (ref.current?.contains(e.target as Node)) return;
      if (anchorRect) {
        const { clientX: x, clientY: y } = e;
        if (x >= anchorRect.left && x <= anchorRect.right && y >= anchorRect.top && y <= anchorRect.bottom) return;
      }
      onClose();
    };
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose();
      }
    };
    // Scroll events don't bubble but capture-phase listeners still see inner
    // scrolls — ignore those so scrolling the popover's own list doesn't close it.
    const onScroll = (e: Event): void => {
      if (ref.current?.contains(e.target as Node)) return;
      onClose();
    };
    const onResize = (): void => onClose();
    window.addEventListener('pointerdown', onDown, true);
    window.addEventListener('keydown', onKey, true);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', onResize, true);
    return () => {
      window.removeEventListener('pointerdown', onDown, true);
      window.removeEventListener('keydown', onKey, true);
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', onResize, true);
    };
  }, [open, onClose, anchorRect]);

  if (!open) return null;

  return createPortal(
    <div
      ref={ref}
      style={{ ...style, maxHeight }}
      className={[
        'flex flex-col overflow-hidden rounded-11px border border-solid border-[var(--color-border-2)]',
        'bg-[var(--color-bg-2)] shadow-[0_12px_36px_rgba(0,0,0,0.24)] backdrop-blur-md',
      ].join(' ')}
      onPointerDown={(e) => e.stopPropagation()}
    >
      {children}
    </div>,
    document.body
  );
};

export default Floating;
