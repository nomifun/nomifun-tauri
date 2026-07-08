/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Mask tool — paint / erase an inpaint mask with undo, then attach a prompt. */
import React, { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input, Slider } from '@arco-design/web-react';
import { ClearFormat, Erase, Paint, Redo, Undo } from '@icon-park/react';
import EditorStage, { type EditorPointerEvent, type Viewport } from '../EditorStage';
import { Field, PanelHint, PanelSection, SegmentedToggle, WorkArea } from '../PanelKit';
import { exportMask } from '../lib/exporters';
import type { ImageToolHandle, ImageToolProps } from '../toolTypes';

type PaintMode = 'brush' | 'erase';
const WORK_MAX = 1600; // longest side of the working paint canvas
const MIN_BRUSH = 8;
const MAX_BRUSH = 160;
const PAINT_RGB = '255,64,64';

interface Stroke {
  mode: PaintMode;
  size: number; // source-px diameter
  points: { x: number; y: number }[]; // source coords
}

const MaskTool = forwardRef<ImageToolHandle, ImageToolProps>(({ image, onCanApplyChange }, ref) => {
  const { t } = useTranslation();
  const { el, naturalWidth: W, naturalHeight: H } = image;
  const k = Math.min(1, WORK_MAX / Math.max(W, H));
  const workW = Math.max(1, Math.round(W * k));
  const workH = Math.max(1, Math.round(H * k));

  const [mode, setMode] = useState<PaintMode>('brush');
  const [brush, setBrush] = useState(40);
  const [prompt, setPrompt] = useState('');
  const [hasPaint, setHasPaint] = useState(false);
  const [canRedo, setCanRedo] = useState(false);
  const [cursor, setCursor] = useState<{ x: number; y: number } | null>(null);

  const paintRef = useRef<HTMLCanvasElement>(null);
  const history = useRef<Stroke[]>([]);
  const redo = useRef<Stroke[]>([]);
  const active = useRef<Stroke | null>(null);
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const brushRef = useRef(brush);
  brushRef.current = brush;

  useEffect(() => {
    onCanApplyChange(hasPaint);
  }, [hasPaint, onCanApplyChange]);

  const ctx = useCallback(() => paintRef.current?.getContext('2d') ?? null, []);

  const drawStroke = useCallback(
    (c: CanvasRenderingContext2D, stroke: Stroke) => {
      c.save();
      c.globalCompositeOperation = stroke.mode === 'erase' ? 'destination-out' : 'source-over';
      c.strokeStyle = `rgba(${PAINT_RGB},1)`;
      c.fillStyle = `rgba(${PAINT_RGB},1)`;
      c.lineCap = 'round';
      c.lineJoin = 'round';
      c.lineWidth = Math.max(1, stroke.size * k);
      const pts = stroke.points;
      if (pts.length === 1) {
        c.beginPath();
        c.arc(pts[0].x * k, pts[0].y * k, c.lineWidth / 2, 0, Math.PI * 2);
        c.fill();
      } else if (pts.length > 1) {
        c.beginPath();
        c.moveTo(pts[0].x * k, pts[0].y * k);
        for (let i = 1; i < pts.length; i += 1) c.lineTo(pts[i].x * k, pts[i].y * k);
        c.stroke();
      }
      c.restore();
    },
    [k]
  );

  const repaint = useCallback(() => {
    const c = ctx();
    if (!c) return;
    c.clearRect(0, 0, workW, workH);
    for (const s of history.current) drawStroke(c, s);
  }, [ctx, drawStroke, workW, workH]);

  // ─── Pointer interaction ─────────────────────────────────────────────────
  const onPointerDown = useCallback(
    (e: EditorPointerEvent) => {
      redo.current = [];
      setCanRedo(false);
      active.current = { mode: modeRef.current, size: brushRef.current, points: [{ x: e.img.x, y: e.img.y }] };
      const c = ctx();
      if (c) drawStroke(c, active.current);
    },
    [ctx, drawStroke]
  );

  const onPointerMove = useCallback(
    (e: EditorPointerEvent) => {
      setCursor({ x: e.screen.x, y: e.screen.y });
      if (!active.current || (e.buttons & 1) === 0) return;
      active.current.points.push({ x: e.img.x, y: e.img.y });
      const c = ctx();
      if (!c) return;
      // Incrementally draw the last segment (cheap; full repaint only on undo).
      c.save();
      c.globalCompositeOperation = active.current.mode === 'erase' ? 'destination-out' : 'source-over';
      c.strokeStyle = `rgba(${PAINT_RGB},1)`;
      c.lineCap = 'round';
      c.lineJoin = 'round';
      c.lineWidth = Math.max(1, active.current.size * k);
      const pts = active.current.points;
      const a = pts[pts.length - 2];
      const b = pts[pts.length - 1];
      c.beginPath();
      c.moveTo(a.x * k, a.y * k);
      c.lineTo(b.x * k, b.y * k);
      c.stroke();
      c.restore();
    },
    [ctx, k]
  );

  const endStroke = useCallback(() => {
    if (active.current) {
      history.current.push(active.current);
      active.current = null;
      setHasPaint(true);
    }
  }, []);

  const undo = useCallback(() => {
    const s = history.current.pop();
    if (!s) return;
    redo.current.push(s);
    setCanRedo(true);
    repaint();
    setHasPaint(history.current.length > 0);
  }, [repaint]);

  const doRedo = useCallback(() => {
    const s = redo.current.pop();
    if (!s) return;
    history.current.push(s);
    setCanRedo(redo.current.length > 0);
    repaint();
    setHasPaint(true);
  }, [repaint]);

  const clear = useCallback(() => {
    history.current = [];
    redo.current = [];
    setCanRedo(false);
    repaint();
    setHasPaint(false);
  }, [repaint]);

  // ─── Apply ────────────────────────────────────────────────────────────────
  useImperativeHandle(ref, () => ({
    apply: async () => {
      const canvas = paintRef.current;
      if (!canvas || !hasPaint) return null;
      const maskBlob = await exportMask(W, H, canvas);
      return { type: 'mask', maskBlob, prompt: prompt.trim() };
    },
  }));

  // ─── Layers ────────────────────────────────────────────────────────────────
  const renderImageLayer = useCallback(
    () => (
      <canvas
        ref={paintRef}
        width={workW}
        height={workH}
        className='absolute left-0 top-0 h-full w-full'
        style={{ opacity: 0.5, mixBlendMode: 'normal' }}
      />
    ),
    [workW, workH]
  );

  const renderScreenLayer = useCallback(
    (vp: Viewport) =>
      cursor ? (
        <div
          className='absolute rounded-full'
          style={{
            left: cursor.x - (brush * vp.scale) / 2,
            top: cursor.y - (brush * vp.scale) / 2,
            width: brush * vp.scale,
            height: brush * vp.scale,
            border: `1.5px solid ${mode === 'erase' ? '#fff' : `rgb(${PAINT_RGB})`}`,
            boxShadow: '0 0 0 1px rgba(0,0,0,0.4)',
          }}
        />
      ) : null,
    [cursor, brush, mode]
  );

  return (
    <WorkArea
      stage={
        <EditorStage
          src={el.src}
          naturalWidth={W}
          naturalHeight={H}
          cursor='none'
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={endStroke}
          onPointerLeave={() => setCursor(null)}
          renderImageLayer={renderImageLayer}
          renderScreenLayer={renderScreenLayer}
        />
      }
      panel={
        <>
          <PanelSection title={t('workshopEditor.mask.tool', { defaultValue: '画笔工具' })}>
            <SegmentedToggle<PaintMode>
              value={mode}
              onChange={setMode}
              options={[
                { value: 'brush', label: t('workshopEditor.mask.brush', { defaultValue: '涂抹' }), icon: <Paint theme='outline' size={15} /> },
                { value: 'erase', label: t('workshopEditor.mask.erase', { defaultValue: '擦除' }), icon: <Erase theme='outline' size={15} /> },
              ]}
            />
            <Field label={t('workshopEditor.mask.brushSize', { defaultValue: '笔刷大小' })} value={`${brush} px`}>
              <Slider min={MIN_BRUSH} max={MAX_BRUSH} value={brush} onChange={(v) => setBrush(v as number)} />
            </Field>
            <div className='flex items-center gap-8px'>
              <IconAction disabled={!hasPaint} title={t('workshopEditor.mask.undo', { defaultValue: '撤销' })} onClick={undo}>
                <Undo theme='outline' size={15} />
              </IconAction>
              <IconAction disabled={!canRedo} title={t('workshopEditor.mask.redo', { defaultValue: '重做' })} onClick={doRedo}>
                <Redo theme='outline' size={15} />
              </IconAction>
              <IconAction disabled={!hasPaint} title={t('workshopEditor.mask.clear', { defaultValue: '清空' })} onClick={clear}>
                <ClearFormat theme='outline' size={15} />
              </IconAction>
            </div>
          </PanelSection>

          <PanelSection title={t('workshopEditor.mask.promptLabel', { defaultValue: '修改要求' })}>
            <Input.TextArea
              value={prompt}
              onChange={setPrompt}
              autoSize={{ minRows: 3, maxRows: 6 }}
              placeholder={t('workshopEditor.mask.promptPlaceholder', { defaultValue: '描述你希望如何修改涂抹的区域…' })}
            />
          </PanelSection>

          <PanelHint>{t('workshopEditor.mask.hint', { defaultValue: '涂抹要重绘的区域（红色）；导出的遮罩中涂抹处为透明、其余为白色。' })}</PanelHint>
        </>
      }
    />
  );
});

MaskTool.displayName = 'MaskTool';

const IconAction: React.FC<React.PropsWithChildren<{ title: string; onClick: () => void; disabled?: boolean }>> = ({ title, onClick, disabled, children }) => (
  <div
    role='button'
    tabIndex={disabled ? -1 : 0}
    title={title}
    onClick={() => !disabled && onClick()}
    onKeyDown={(e) => {
      if (!disabled && (e.key === 'Enter' || e.key === ' ')) {
        e.preventDefault();
        onClick();
      }
    }}
    className='grid h-32px flex-1 place-items-center rounded-8px transition-colors'
    style={{
      border: '1px solid var(--nfe-panel-border)',
      background: 'var(--nfe-inset-bg)',
      color: disabled ? 'var(--nfe-text-3)' : 'var(--nfe-text-1)',
      cursor: disabled ? 'not-allowed' : 'pointer',
      opacity: disabled ? 0.5 : 1,
    }}
  >
    {children}
  </div>
);

export default MaskTool;
