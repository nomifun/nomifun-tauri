/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Crop tool — draggable frame with 8 handles and aspect-ratio presets. */
import React, { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import EditorStage, { type EditorPointerEvent, type Viewport } from '../EditorStage';
import { PanelHint, PanelSection, StatPill, WorkArea } from '../PanelKit';
import { exportCrop, type Rect } from '../lib/exporters';
import type { ImageToolHandle, ImageToolProps } from '../toolTypes';

type AspectKey = 'free' | 'original' | '1:1' | '4:3' | '3:4' | '16:9' | '9:16';
type Handle = 'nw' | 'n' | 'ne' | 'e' | 'se' | 's' | 'sw' | 'w' | 'move' | 'new';

const MIN_CROP = 8; // source px

function aspectValue(key: AspectKey, natW: number, natH: number): number | null {
  switch (key) {
    case 'free':
      return null;
    case 'original':
      return natW / natH;
    case '1:1':
      return 1;
    case '4:3':
      return 4 / 3;
    case '3:4':
      return 3 / 4;
    case '16:9':
      return 16 / 9;
    case '9:16':
      return 9 / 16;
    default:
      return null;
  }
}

function clampRectToBounds(rect: Rect, w: number, h: number): Rect {
  const width = Math.min(rect.w, w);
  const height = Math.min(rect.h, h);
  const x = Math.max(0, Math.min(rect.x, w - width));
  const y = Math.max(0, Math.min(rect.y, h - height));
  return { x, y, w: width, h: height };
}

/** Centre a rect of the given aspect at ~80% of the image. */
function fitRect(natW: number, natH: number, aspect: number | null): Rect {
  if (aspect === null) {
    const w = natW * 0.8;
    const h = natH * 0.8;
    return { x: (natW - w) / 2, y: (natH - h) / 2, w, h };
  }
  let w = natW * 0.8;
  let h = w / aspect;
  if (h > natH * 0.9) {
    h = natH * 0.9;
    w = h * aspect;
  }
  return { x: (natW - w) / 2, y: (natH - h) / 2, w, h };
}

const CropTool = forwardRef<ImageToolHandle, ImageToolProps>(({ image, onCanApplyChange }, ref) => {
  const { t } = useTranslation();
  const { el, naturalWidth: W, naturalHeight: H } = image;
  const [aspect, setAspect] = useState<AspectKey>('free');
  const [rect, setRect] = useState<Rect>(() => fitRect(W, H, null));
  const rectRef = useRef(rect);
  rectRef.current = rect;

  const drag = useRef<{ handle: Handle; start: Rect; startImg: { x: number; y: number } } | null>(null);

  useEffect(() => {
    onCanApplyChange(rect.w >= 1 && rect.h >= 1);
  }, [rect.w, rect.h, onCanApplyChange]);

  const applyAspect = useCallback(
    (key: AspectKey) => {
      setAspect(key);
      const a = aspectValue(key, W, H);
      if (a !== null) setRect(clampRectToBounds(fitRect(W, H, a), W, H));
    },
    [W, H]
  );

  // ─── Pointer interaction ─────────────────────────────────────────────────
  const hitTest = useCallback((e: EditorPointerEvent): Handle => {
    const r = rectRef.current;
    const tol = 10 / e.vp.scale; // 10 screen px in image units
    const { x, y } = e.img;
    const nearL = Math.abs(x - r.x) <= tol;
    const nearR = Math.abs(x - (r.x + r.w)) <= tol;
    const nearT = Math.abs(y - r.y) <= tol;
    const nearB = Math.abs(y - (r.y + r.h)) <= tol;
    const inX = x >= r.x - tol && x <= r.x + r.w + tol;
    const inY = y >= r.y - tol && y <= r.y + r.h + tol;
    if (inX && inY) {
      if (nearT && nearL) return 'nw';
      if (nearT && nearR) return 'ne';
      if (nearB && nearL) return 'sw';
      if (nearB && nearR) return 'se';
      if (nearT) return 'n';
      if (nearB) return 's';
      if (nearL) return 'w';
      if (nearR) return 'e';
      if (x > r.x && x < r.x + r.w && y > r.y && y < r.y + r.h) return 'move';
    }
    return 'new';
  }, []);

  const onPointerDown = useCallback(
    (e: EditorPointerEvent) => {
      const handle = hitTest(e);
      if (handle === 'new') {
        const start = { x: e.img.x, y: e.img.y, w: 0, h: 0 };
        setRect(start);
        drag.current = { handle: 'se', start, startImg: { x: e.img.x, y: e.img.y } };
      } else {
        drag.current = { handle, start: rectRef.current, startImg: { x: e.img.x, y: e.img.y } };
      }
    },
    [hitTest]
  );

  const onPointerMove = useCallback(
    (e: EditorPointerEvent) => {
      const d = drag.current;
      if (!d) return;
      const a = aspectValue(aspect, W, H);
      const dx = e.img.x - d.startImg.x;
      const dy = e.img.y - d.startImg.y;
      setRect(resizeRect(d.start, d.handle, dx, dy, e.img, a, W, H));
    },
    [aspect, W, H]
  );

  const onPointerUp = useCallback(() => {
    drag.current = null;
  }, []);

  // ─── Apply ────────────────────────────────────────────────────────────────
  useImperativeHandle(ref, () => ({
    apply: async () => {
      const r = clampRectToBounds(rectRef.current, W, H);
      const blob = await exportCrop(el, r, W, H);
      return { type: 'crop', blob };
    },
  }));

  // ─── Overlay (screen space) ─────────────────────────────────────────────
  const renderScreenLayer = useCallback(
    (vp: Viewport) => <CropOverlay rect={rect} vp={vp} />,
    [rect]
  );

  const presets: AspectKey[] = ['free', 'original', '1:1', '4:3', '3:4', '16:9', '9:16'];
  const presetLabel = (k: AspectKey): string => {
    if (k === 'free') return t('workshopEditor.crop.free', { defaultValue: '自由' });
    if (k === 'original') return t('workshopEditor.crop.original', { defaultValue: '原图' });
    return k;
  };

  return (
    <WorkArea
      stage={
        <EditorStage
          src={el.src}
          naturalWidth={W}
          naturalHeight={H}
          cursor='crosshair'
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          renderScreenLayer={renderScreenLayer}
        />
      }
      panel={
        <>
          <PanelSection title={t('workshopEditor.crop.aspect', { defaultValue: '裁剪比例' })}>
            <div className='grid grid-cols-3 gap-6px'>
              {presets.map((k) => {
                const active = k === aspect;
                return (
                  <div
                    key={k}
                    role='button'
                    tabIndex={0}
                    onClick={() => applyAspect(k)}
                    onKeyDown={(ev) => {
                      if (ev.key === 'Enter' || ev.key === ' ') {
                        ev.preventDefault();
                        applyAspect(k);
                      }
                    }}
                    className='flex h-32px cursor-pointer items-center justify-center rounded-8px text-12px transition-all'
                    style={{
                      border: '1px solid ' + (active ? 'var(--nfe-accent)' : 'var(--nfe-panel-border)'),
                      background: active ? 'var(--nfe-accent-soft)' : 'var(--nfe-inset-bg)',
                      color: active ? 'var(--nfe-accent)' : 'var(--nfe-text-2)',
                      fontWeight: active ? 600 : 400,
                    }}
                  >
                    {presetLabel(k)}
                  </div>
                );
              })}
            </div>
          </PanelSection>

          <PanelSection title={t('workshopEditor.crop.size', { defaultValue: '裁剪尺寸' })}>
            <div className='grid grid-cols-2 gap-8px'>
              <StatPill label={t('workshopEditor.crop.width', { defaultValue: '宽度' })} value={`${Math.round(rect.w)} px`} />
              <StatPill label={t('workshopEditor.crop.height', { defaultValue: '高度' })} value={`${Math.round(rect.h)} px`} />
            </div>
            <div className='flex items-center justify-between text-12px'>
              <span style={{ color: 'var(--nfe-text-3)' }}>{t('workshopEditor.crop.position', { defaultValue: '位置' })}</span>
              <span className='tabular-nums font-600' style={{ color: 'var(--nfe-text-2)' }}>
                {Math.round(rect.x)}, {Math.round(rect.y)}
              </span>
            </div>
          </PanelSection>

          <PanelHint>{t('workshopEditor.crop.hint', { defaultValue: '拖动裁剪框移动，拖动手柄调整大小；在框外按下可重新框选。' })}</PanelHint>
        </>
      }
    />
  );
});

CropTool.displayName = 'CropTool';

// ─── Resize math ──────────────────────────────────────────────────────────────

function resizeRect(start: Rect, handle: Handle, dx: number, dy: number, ptr: { x: number; y: number }, aspect: number | null, W: number, H: number): Rect {
  if (handle === 'move') {
    let x = start.x + dx;
    let y = start.y + dy;
    x = Math.max(0, Math.min(x, W - start.w));
    y = Math.max(0, Math.min(y, H - start.h));
    return { x, y, w: start.w, h: start.h };
  }

  let left = start.x;
  let top = start.y;
  let right = start.x + start.w;
  let bottom = start.y + start.h;
  const movesL = handle === 'nw' || handle === 'w' || handle === 'sw';
  const movesR = handle === 'ne' || handle === 'e' || handle === 'se';
  const movesT = handle === 'nw' || handle === 'n' || handle === 'ne';
  const movesB = handle === 'sw' || handle === 's' || handle === 'se';

  if (movesL) left = Math.min(ptr.x, right - MIN_CROP);
  if (movesR) right = Math.max(ptr.x, left + MIN_CROP);
  if (movesT) top = Math.min(ptr.y, bottom - MIN_CROP);
  if (movesB) bottom = Math.max(ptr.y, top + MIN_CROP);

  let rect: Rect = { x: left, y: top, w: right - left, h: bottom - top };

  if (aspect !== null) {
    rect = enforceAspect(rect, handle, aspect, start);
  }
  return clampRectToBounds(rect, W, H);
}

function enforceAspect(rect: Rect, handle: Handle, aspect: number, start: Rect): Rect {
  const isCorner = handle === 'nw' || handle === 'ne' || handle === 'sw' || handle === 'se';
  const isHorizEdge = handle === 'e' || handle === 'w';
  let { x, y, w, h } = rect;

  if (isCorner) {
    // Drive height from the (freshly resized) width.
    h = w / aspect;
    // Anchor at the corner opposite the dragged one.
    if (handle === 'nw' || handle === 'ne') y = start.y + start.h - h;
    if (handle === 'nw' || handle === 'sw') x = start.x + start.w - w;
  } else if (isHorizEdge) {
    // Width changed; grow height about the vertical centre.
    h = w / aspect;
    const cy = start.y + start.h / 2;
    y = cy - h / 2;
  } else {
    // Vertical edge: height changed; grow width about the horizontal centre.
    w = h * aspect;
    const cx = start.x + start.w / 2;
    x = cx - w / 2;
  }
  return { x, y, w, h };
}

// ─── Overlay renderer ─────────────────────────────────────────────────────────

const CropOverlay: React.FC<{ rect: Rect; vp: Viewport }> = ({ rect, vp }) => {
  const sx = rect.x * vp.scale + vp.offsetX;
  const sy = rect.y * vp.scale + vp.offsetY;
  const sw = rect.w * vp.scale;
  const sh = rect.h * vp.scale;
  const handleKeys: { key: string; left: number; top: number }[] = [
    { key: 'nw', left: sx, top: sy },
    { key: 'n', left: sx + sw / 2, top: sy },
    { key: 'ne', left: sx + sw, top: sy },
    { key: 'e', left: sx + sw, top: sy + sh / 2 },
    { key: 'se', left: sx + sw, top: sy + sh },
    { key: 's', left: sx + sw / 2, top: sy + sh },
    { key: 'sw', left: sx, top: sy + sh },
    { key: 'w', left: sx, top: sy + sh / 2 },
  ];
  return (
    <div className='pointer-events-none absolute inset-0'>
      {/* Dark scrim outside the crop rect (4 bands) */}
      <div className='absolute' style={{ left: 0, top: 0, right: 0, height: Math.max(0, sy), background: 'var(--nfe-scrim)' }} />
      <div className='absolute' style={{ left: 0, top: sy + sh, right: 0, bottom: 0, background: 'var(--nfe-scrim)' }} />
      <div className='absolute' style={{ left: 0, top: sy, width: Math.max(0, sx), height: sh, background: 'var(--nfe-scrim)' }} />
      <div className='absolute' style={{ left: sx + sw, top: sy, right: 0, height: sh, background: 'var(--nfe-scrim)' }} />

      {/* Frame */}
      <div className='absolute' style={{ left: sx, top: sy, width: sw, height: sh, outline: '1.5px solid var(--nfe-accent)', boxShadow: '0 0 0 1px rgba(0,0,0,0.35)' }}>
        {/* Rule-of-thirds guides */}
        <div className='absolute' style={{ left: '33.33%', top: 0, bottom: 0, width: 1, background: 'rgba(255,255,255,0.28)' }} />
        <div className='absolute' style={{ left: '66.66%', top: 0, bottom: 0, width: 1, background: 'rgba(255,255,255,0.28)' }} />
        <div className='absolute' style={{ top: '33.33%', left: 0, right: 0, height: 1, background: 'rgba(255,255,255,0.28)' }} />
        <div className='absolute' style={{ top: '66.66%', left: 0, right: 0, height: 1, background: 'rgba(255,255,255,0.28)' }} />
        {/* Size badge */}
        <div
          className='absolute left-1/2 -translate-x-1/2 whitespace-nowrap rounded-6px px-7px py-3px text-11px font-600 tabular-nums'
          style={{ top: -26, background: 'var(--nfe-accent)', color: '#fff' }}
        >
          {Math.round(rect.w)} × {Math.round(rect.h)}
        </div>
      </div>

      {/* Handles */}
      {handleKeys.map((h) => (
        <div
          key={h.key}
          className='absolute rounded-3px'
          style={{
            left: h.left - 5,
            top: h.top - 5,
            width: 10,
            height: 10,
            background: '#fff',
            border: '1.5px solid var(--nfe-accent)',
            boxShadow: '0 1px 3px rgba(0,0,0,0.4)',
          }}
        />
      ))}
    </div>
  );
};

export default CropTool;
