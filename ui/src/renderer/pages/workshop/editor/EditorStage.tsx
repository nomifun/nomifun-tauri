/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * EditorStage — the shared, dark canvas workspace for every image-editor tool.
 *
 * Owns pan / zoom / checkerboard rendering and converts pointer events into
 * source-image coordinates. Tools stay purely declarative: they draw their
 * overlays through {@link EditorStageProps.renderScreenLayer} (screen space,
 * for crisp handles) and {@link EditorStageProps.renderImageLayer} (image space,
 * for the mask canvas), and receive already-converted pointer events.
 */
import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Aiming, Move, ZoomIn, ZoomOut } from '@icon-park/react';

/** Screen ↔ image transform: `screen = image * scale + offset`. */
export interface Viewport {
  scale: number;
  offsetX: number;
  offsetY: number;
  naturalWidth: number;
  naturalHeight: number;
}

/** A pointer event already resolved into image + stage-screen coordinates. */
export interface EditorPointerEvent {
  /** Source-image pixel coordinates (may fall outside `[0, natural]`). */
  img: { x: number; y: number };
  /** Coordinates relative to the stage container, in CSS px. */
  screen: { x: number; y: number };
  vp: Viewport;
  buttons: number;
  shiftKey: boolean;
  altKey: boolean;
  pointerId: number;
}

export interface EditorStageProps {
  src: string;
  naturalWidth: number;
  naturalHeight: number;
  /** CSS cursor for the interaction surface when not panning. */
  cursor?: string;
  onPointerDown?: (e: EditorPointerEvent) => void;
  onPointerMove?: (e: EditorPointerEvent) => void;
  onPointerUp?: (e: EditorPointerEvent) => void;
  onPointerLeave?: () => void;
  /** Image-space children (transformed with the image); pointer-inert. */
  renderImageLayer?: (vp: Viewport) => React.ReactNode;
  /** Screen-space children (constant size); pointer-inert. */
  renderScreenLayer?: (vp: Viewport) => React.ReactNode;
}

const MIN_SCALE = 0.02;
const MAX_SCALE = 32;
const FIT_PADDING = 48;

function clampScale(scale: number): number {
  return Math.max(MIN_SCALE, Math.min(MAX_SCALE, scale));
}

const EditorStage: React.FC<EditorStageProps> = ({
  src,
  naturalWidth,
  naturalHeight,
  cursor = 'default',
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerLeave,
  renderImageLayer,
  renderScreenLayer,
}) => {
  const { t } = useTranslation();
  const containerRef = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [vp, setVp] = useState<Viewport>({ scale: 0, offsetX: 0, offsetY: 0, naturalWidth, naturalHeight });
  const vpRef = useRef(vp);
  vpRef.current = vp;

  const userAdjusted = useRef(false);
  const spaceHeld = useRef(false);
  const panning = useRef<{ pointerId: number; startX: number; startY: number; ox: number; oy: number } | null>(null);
  const [isPanning, setIsPanning] = useState(false);

  // ─── Container size tracking ───────────────────────────────────────────────
  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (rect) setSize({ w: rect.width, h: rect.height });
    });
    ro.observe(el);
    const rect = el.getBoundingClientRect();
    setSize({ w: rect.width, h: rect.height });
    return () => ro.disconnect();
  }, []);

  const fit = useCallback((): Viewport => {
    const availW = Math.max(1, size.w - FIT_PADDING * 2);
    const availH = Math.max(1, size.h - FIT_PADDING * 2);
    const scale = clampScale(Math.min(availW / naturalWidth, availH / naturalHeight, 1));
    return {
      scale,
      offsetX: (size.w - naturalWidth * scale) / 2,
      offsetY: (size.h - naturalHeight * scale) / 2,
      naturalWidth,
      naturalHeight,
    };
  }, [size.w, size.h, naturalWidth, naturalHeight]);

  // Fit on first layout / natural change, and re-fit on resize until the user
  // manually zooms or pans.
  useEffect(() => {
    if (size.w === 0 || size.h === 0) return;
    if (!userAdjusted.current || vpRef.current.scale === 0) setVp(fit());
  }, [fit, size.w, size.h]);

  const resetView = useCallback(() => {
    userAdjusted.current = false;
    setVp(fit());
  }, [fit]);

  const zoomAt = useCallback(
    (factor: number, anchorX: number, anchorY: number) => {
      userAdjusted.current = true;
      setVp((prev) => {
        const scale = clampScale(prev.scale * factor);
        const imgX = (anchorX - prev.offsetX) / prev.scale;
        const imgY = (anchorY - prev.offsetY) / prev.scale;
        return {
          ...prev,
          scale,
          offsetX: anchorX - imgX * scale,
          offsetY: anchorY - imgY * scale,
        };
      });
    },
    []
  );

  const zoomButton = useCallback(
    (factor: number) => {
      zoomAt(factor, size.w / 2, size.h / 2);
    },
    [zoomAt, size.w, size.h]
  );

  // ─── Wheel zoom (native, non-passive so we can preventDefault) ─────────────
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const onWheel = (ev: WheelEvent) => {
      ev.preventDefault();
      const rect = el.getBoundingClientRect();
      const factor = ev.deltaY < 0 ? 1.12 : 1 / 1.12;
      zoomAt(factor, ev.clientX - rect.left, ev.clientY - rect.top);
    };
    el.addEventListener('wheel', onWheel, { passive: false });
    return () => el.removeEventListener('wheel', onWheel);
  }, [zoomAt]);

  // ─── Space-to-pan modifier ─────────────────────────────────────────────────
  useEffect(() => {
    const down = (ev: KeyboardEvent) => {
      if (ev.code === 'Space' && !isEditableTarget(ev.target)) {
        spaceHeld.current = true;
      }
    };
    const up = (ev: KeyboardEvent) => {
      if (ev.code === 'Space') spaceHeld.current = false;
    };
    window.addEventListener('keydown', down);
    window.addEventListener('keyup', up);
    return () => {
      window.removeEventListener('keydown', down);
      window.removeEventListener('keyup', up);
    };
  }, []);

  const toEvent = useCallback((ev: React.PointerEvent): EditorPointerEvent => {
    const rect = containerRef.current?.getBoundingClientRect();
    const sx = ev.clientX - (rect?.left ?? 0);
    const sy = ev.clientY - (rect?.top ?? 0);
    const cur = vpRef.current;
    return {
      img: { x: (sx - cur.offsetX) / cur.scale, y: (sy - cur.offsetY) / cur.scale },
      screen: { x: sx, y: sy },
      vp: cur,
      buttons: ev.buttons,
      shiftKey: ev.shiftKey,
      altKey: ev.altKey,
      pointerId: ev.pointerId,
    };
  }, []);

  const handlePointerDown = useCallback(
    (ev: React.PointerEvent) => {
      // Keep receiving move/up even if the pointer briefly leaves the surface.
      try {
        ev.currentTarget.setPointerCapture(ev.pointerId);
      } catch {
        /* capture is best-effort */
      }
      const wantPan = spaceHeld.current || ev.button === 1;
      if (wantPan) {
        panning.current = {
          pointerId: ev.pointerId,
          startX: ev.clientX,
          startY: ev.clientY,
          ox: vpRef.current.offsetX,
          oy: vpRef.current.offsetY,
        };
        setIsPanning(true);
        return;
      }
      onPointerDown?.(toEvent(ev));
    },
    [onPointerDown, toEvent]
  );

  const handlePointerMove = useCallback(
    (ev: React.PointerEvent) => {
      const pan = panning.current;
      if (pan && pan.pointerId === ev.pointerId) {
        userAdjusted.current = true;
        const dx = ev.clientX - pan.startX;
        const dy = ev.clientY - pan.startY;
        setVp((prev) => ({ ...prev, offsetX: pan.ox + dx, offsetY: pan.oy + dy }));
        return;
      }
      onPointerMove?.(toEvent(ev));
    },
    [onPointerMove, toEvent]
  );

  const endPan = useCallback((ev: React.PointerEvent) => {
    try {
      ev.currentTarget.releasePointerCapture(ev.pointerId);
    } catch {
      /* capture may already be gone */
    }
    const pan = panning.current;
    if (pan && pan.pointerId === ev.pointerId) {
      panning.current = null;
      setIsPanning(false);
      return true;
    }
    return false;
  }, []);

  const handlePointerUp = useCallback(
    (ev: React.PointerEvent) => {
      if (endPan(ev)) return;
      onPointerUp?.(toEvent(ev));
    },
    [endPan, onPointerUp, toEvent]
  );

  const handlePointerLeave = useCallback(() => {
    onPointerLeave?.();
  }, [onPointerLeave]);

  const effectiveCursor = isPanning ? 'grabbing' : cursor;

  return (
    <div
      ref={containerRef}
      className='relative h-full w-full overflow-hidden select-none'
      style={{ background: 'var(--nfe-stage-bg)' }}
    >
      {/* Transparency checkerboard (screen-space) */}
      <div className='pointer-events-none absolute inset-0' style={{ backgroundImage: 'var(--nfe-checker)', backgroundSize: '22px 22px' }} />

      {/* Transformed image layer */}
      {vp.scale > 0 && (
        <div
          className='pointer-events-none absolute left-0 top-0'
          style={{
            width: naturalWidth,
            height: naturalHeight,
            transformOrigin: '0 0',
            transform: `translate(${vp.offsetX}px, ${vp.offsetY}px) scale(${vp.scale})`,
          }}
        >
          <img src={src} alt='' draggable={false} className='block h-full w-full' style={{ imageRendering: vp.scale > 3 ? 'pixelated' : 'auto' }} />
          {renderImageLayer?.(vp)}
        </div>
      )}

      {/* Screen-space overlay (handles, guides, brush cursor) */}
      {vp.scale > 0 && <div className='pointer-events-none absolute inset-0'>{renderScreenLayer?.(vp)}</div>}

      {/* Interaction surface */}
      <div
        className='absolute inset-0'
        style={{ cursor: effectiveCursor, touchAction: 'none' }}
        onPointerDown={handlePointerDown}
        onPointerMove={handlePointerMove}
        onPointerUp={handlePointerUp}
        onPointerCancel={handlePointerUp}
        onPointerLeave={handlePointerLeave}
      />

      {/* Zoom controls (bottom-left) */}
      <div
        className='absolute bottom-14px left-14px flex items-center gap-2px rounded-10px border border-solid px-4px py-4px backdrop-blur-md'
        style={{ borderColor: 'var(--nfe-stage-border)', background: 'var(--nfe-toolbar-bg)' }}
      >
        <StageIconButton title={t('workshopEditor.stage.zoomOut', { defaultValue: '缩小' })} onClick={() => zoomButton(1 / 1.2)}>
          <ZoomOut theme='outline' size={16} />
        </StageIconButton>
        <div className='w-46px text-center text-12px tabular-nums' style={{ color: 'var(--nfe-stage-text)' }}>
          {Math.round(vp.scale * 100)}%
        </div>
        <StageIconButton title={t('workshopEditor.stage.zoomIn', { defaultValue: '放大' })} onClick={() => zoomButton(1.2)}>
          <ZoomIn theme='outline' size={16} />
        </StageIconButton>
        <div className='mx-2px h-16px w-1px' style={{ background: 'var(--nfe-stage-border)' }} />
        <StageIconButton title={t('workshopEditor.stage.resetView', { defaultValue: '适应窗口' })} onClick={resetView}>
          <Aiming theme='outline' size={16} />
        </StageIconButton>
      </div>

      {/* Pan hint (bottom-right) */}
      <div
        className='pointer-events-none absolute bottom-14px right-14px flex items-center gap-6px rounded-8px px-9px py-5px text-11px'
        style={{ background: 'var(--nfe-toolbar-bg)', color: 'var(--nfe-stage-text-dim)' }}
      >
        <Move theme='outline' size={13} />
        {t('workshopEditor.stage.hint', { defaultValue: '滚轮缩放 · 空格拖动查看' })}
      </div>
    </div>
  );
};

function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || target.isContentEditable;
}

const StageIconButton: React.FC<React.PropsWithChildren<{ title: string; onClick: () => void }>> = ({ title, onClick, children }) => (
  <div
    role='button'
    tabIndex={0}
    title={title}
    onClick={onClick}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onClick();
      }
    }}
    className='grid h-26px w-26px cursor-pointer place-items-center rounded-7px transition-colors'
    style={{ color: 'var(--nfe-stage-text)' }}
    onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--nfe-stage-hover)')}
    onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
  >
    {children}
  </div>
);

export default EditorStage;
