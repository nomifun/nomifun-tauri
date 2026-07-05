/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Split tool — even grid or custom draggable lines, with seam-gap removal. */
import React, { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { InputNumber, Slider } from '@arco-design/web-react';
import { Delete, DividingLine, GridNine, Plus } from '@icon-park/react';
import EditorStage, { type EditorPointerEvent, type Viewport } from '../EditorStage';
import { Field, PanelHint, PanelSection, SegmentedToggle, StatPill, WorkArea } from '../PanelKit';
import { computeBands, equalDividers, exportSplit, type Divider } from '../lib/exporters';
import type { ImageToolHandle, ImageToolProps } from '../toolTypes';

type SplitMode = 'equal' | 'custom';
type Orient = 'v' | 'h';
interface CustomLine {
  id: number;
  orient: Orient;
  pos: number; // source px
  gap: number; // source px
}

const MAX_GAP = 64;
const MAX_DIV = 12;
const HIT_TOL = 8; // screen px

const SplitTool = forwardRef<ImageToolHandle, ImageToolProps>(({ image, onCanApplyChange }, ref) => {
  const { t } = useTranslation();
  const { el, naturalWidth: W, naturalHeight: H } = image;

  const [mode, setMode] = useState<SplitMode>('equal');
  const [rows, setRows] = useState(2);
  const [cols, setCols] = useState(2);
  const [gap, setGap] = useState(0);
  const [lines, setLines] = useState<CustomLine[]>([]);
  const [selected, setSelected] = useState<number | null>(null);
  const lineSeq = useRef(1);

  const stateRef = useRef({ mode, lines });
  stateRef.current = { mode, lines };
  const drag = useRef<{ id: number } | null>(null);

  // ─── Divider derivation ─────────────────────────────────────────────────
  const { xDividers, yDividers } = useMemo<{ xDividers: Divider[]; yDividers: Divider[] }>(() => {
    if (mode === 'equal') {
      return { xDividers: equalDividers(W, cols, gap), yDividers: equalDividers(H, rows, gap) };
    }
    const xs = lines.filter((l) => l.orient === 'v').map((l) => ({ pos: l.pos, gap: l.gap }));
    const ys = lines.filter((l) => l.orient === 'h').map((l) => ({ pos: l.pos, gap: l.gap }));
    return { xDividers: xs, yDividers: ys };
  }, [mode, rows, cols, gap, lines, W, H]);

  const bandsX = useMemo(() => computeBands(W, xDividers), [W, xDividers]);
  const bandsY = useMemo(() => computeBands(H, yDividers), [H, yDividers]);
  const pieceCount = bandsX.length * bandsY.length;
  const firstCell = bandsX.length > 0 && bandsY.length > 0
    ? { w: Math.round(bandsX[0].end - bandsX[0].start), h: Math.round(bandsY[0].end - bandsY[0].start) }
    : { w: 0, h: 0 };

  useEffect(() => {
    onCanApplyChange(pieceCount > 0);
  }, [pieceCount, onCanApplyChange]);

  // ─── Custom-line editing ─────────────────────────────────────────────────
  const addLine = useCallback(
    (orient: Orient) => {
      const id = lineSeq.current++;
      setLines((prev) => [...prev, { id, orient, pos: orient === 'v' ? W / 2 : H / 2, gap: 0 }]);
      setSelected(id);
    },
    [W, H]
  );

  const updateLine = useCallback((id: number, patch: Partial<CustomLine>) => {
    setLines((prev) => prev.map((l) => (l.id === id ? { ...l, ...patch } : l)));
  }, []);

  const removeLine = useCallback((id: number) => {
    setLines((prev) => prev.filter((l) => l.id !== id));
    setSelected((s) => (s === id ? null : s));
  }, []);

  const onPointerDown = useCallback(
    (e: EditorPointerEvent) => {
      if (stateRef.current.mode !== 'custom') return;
      const tol = HIT_TOL / e.vp.scale;
      let best: { id: number; dist: number } | null = null;
      for (const l of stateRef.current.lines) {
        const dist = l.orient === 'v' ? Math.abs(e.img.x - l.pos) : Math.abs(e.img.y - l.pos);
        if (dist <= tol && (!best || dist < best.dist)) best = { id: l.id, dist };
      }
      if (best) {
        drag.current = { id: best.id };
        setSelected(best.id);
      } else {
        setSelected(null);
      }
    },
    []
  );

  const onPointerMove = useCallback(
    (e: EditorPointerEvent) => {
      const d = drag.current;
      if (!d) return;
      const line = stateRef.current.lines.find((l) => l.id === d.id);
      if (!line) return;
      const pos = line.orient === 'v' ? Math.max(1, Math.min(W - 1, e.img.x)) : Math.max(1, Math.min(H - 1, e.img.y));
      updateLine(d.id, { pos });
    },
    [W, H, updateLine]
  );

  const onPointerUp = useCallback(() => {
    drag.current = null;
  }, []);

  // ─── Apply ────────────────────────────────────────────────────────────────
  useImperativeHandle(ref, () => ({
    apply: async () => {
      const pieces = await exportSplit(el, W, H, xDividers, yDividers);
      return { type: 'split', pieces };
    },
  }));

  // ─── Overlay ────────────────────────────────────────────────────────────────
  const renderScreenLayer = useCallback(
    (vp: Viewport) => (
      <SplitOverlay
        vp={vp}
        naturalWidth={W}
        naturalHeight={H}
        xDividers={xDividers}
        yDividers={yDividers}
        lines={mode === 'custom' ? lines : []}
        selected={selected}
      />
    ),
    [W, H, xDividers, yDividers, lines, mode, selected]
  );

  const vLines = lines.filter((l) => l.orient === 'v');
  const hLines = lines.filter((l) => l.orient === 'h');

  return (
    <WorkArea
      stage={
        <EditorStage
          src={el.src}
          naturalWidth={W}
          naturalHeight={H}
          cursor={mode === 'custom' ? 'pointer' : 'default'}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          renderScreenLayer={renderScreenLayer}
        />
      }
      panel={
        <>
          <PanelSection title={t('workshopEditor.split.mode', { defaultValue: '切分方式' })}>
            <SegmentedToggle<SplitMode>
              value={mode}
              onChange={setMode}
              options={[
                { value: 'equal', label: t('workshopEditor.split.equal', { defaultValue: '等分' }), icon: <GridNine theme='outline' size={15} /> },
                { value: 'custom', label: t('workshopEditor.split.custom', { defaultValue: '自定义' }), icon: <DividingLine theme='outline' size={15} /> },
              ]}
            />
          </PanelSection>

          {mode === 'equal' ? (
            <PanelSection title={t('workshopEditor.split.grid', { defaultValue: '网格' })}>
              <div className='grid grid-cols-2 gap-8px'>
                <Field label={t('workshopEditor.split.rows', { defaultValue: '行' })}>
                  <InputNumber mode='button' min={1} max={MAX_DIV} value={rows} onChange={(v) => setRows(clampInt(v, 1, MAX_DIV))} />
                </Field>
                <Field label={t('workshopEditor.split.cols', { defaultValue: '列' })}>
                  <InputNumber mode='button' min={1} max={MAX_DIV} value={cols} onChange={(v) => setCols(clampInt(v, 1, MAX_DIV))} />
                </Field>
              </div>
              <Field label={t('workshopEditor.split.gap', { defaultValue: '接缝间隔' })} value={`${gap} px`}>
                <Slider min={0} max={MAX_GAP} value={gap} onChange={(v) => setGap(v as number)} />
              </Field>
            </PanelSection>
          ) : (
            <PanelSection title={t('workshopEditor.split.lines', { defaultValue: '分割线' })}>
              <div className='grid grid-cols-2 gap-8px'>
                <AddButton onClick={() => addLine('v')} label={t('workshopEditor.split.addVertical', { defaultValue: '垂直线' })} />
                <AddButton onClick={() => addLine('h')} label={t('workshopEditor.split.addHorizontal', { defaultValue: '水平线' })} />
              </div>
              {lines.length === 0 ? (
                <p className='m-0 text-12px' style={{ color: 'var(--nfe-text-3)' }}>
                  {t('workshopEditor.split.noLines', { defaultValue: '尚未添加分割线' })}
                </p>
              ) : (
                <div className='flex flex-col gap-8px'>
                  {[...vLines, ...hLines].map((l) => (
                    <LineRow
                      key={l.id}
                      line={l}
                      selected={selected === l.id}
                      maxPos={l.orient === 'v' ? W : H}
                      label={l.orient === 'v' ? t('workshopEditor.split.vLine', { defaultValue: '垂直' }) : t('workshopEditor.split.hLine', { defaultValue: '水平' })}
                      gapLabel={t('workshopEditor.split.lineGap', { defaultValue: '间隔' })}
                      onSelect={() => setSelected(l.id)}
                      onGap={(g) => updateLine(l.id, { gap: g })}
                      onDelete={() => removeLine(l.id)}
                    />
                  ))}
                </div>
              )}
            </PanelSection>
          )}

          <PanelSection title={t('workshopEditor.split.result', { defaultValue: '切分结果' })}>
            <div className='grid grid-cols-2 gap-8px'>
              <StatPill label={t('workshopEditor.split.pieces', { defaultValue: '块数' })} value={String(pieceCount)} />
              <StatPill label={t('workshopEditor.split.pieceSize', { defaultValue: '单块尺寸' })} value={`${firstCell.w}×${firstCell.h}`} />
            </div>
          </PanelSection>

          <PanelHint>
            {mode === 'equal'
              ? t('workshopEditor.split.hintEqual', { defaultValue: '按行列等分；接缝间隔会在每条分割线两侧各裁掉一半，去除 AI 宫格图的拼接缝。' })
              : t('workshopEditor.split.hintCustom', { defaultValue: '添加分割线后可在画布上拖动微调；每条线可单独设置接缝间隔。' })}
          </PanelHint>
        </>
      }
    />
  );
});

SplitTool.displayName = 'SplitTool';

function clampInt(v: number | undefined, min: number, max: number): number {
  const n = Math.round(v ?? min);
  return Math.max(min, Math.min(max, Number.isFinite(n) ? n : min));
}

// ─── Panel sub-components ─────────────────────────────────────────────────────

const AddButton: React.FC<{ onClick: () => void; label: string }> = ({ onClick, label }) => (
  <div
    role='button'
    tabIndex={0}
    onClick={onClick}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onClick();
      }
    }}
    className='flex h-32px cursor-pointer items-center justify-center gap-5px rounded-8px text-12px transition-colors'
    style={{ border: '1px dashed var(--nfe-panel-border)', color: 'var(--nfe-text-2)' }}
  >
    <Plus theme='outline' size={13} />
    {label}
  </div>
);

const LineRow: React.FC<{
  line: CustomLine;
  selected: boolean;
  maxPos: number;
  label: string;
  gapLabel: string;
  onSelect: () => void;
  onGap: (gap: number) => void;
  onDelete: () => void;
}> = ({ line, selected, maxPos, label, gapLabel, onSelect, onGap, onDelete }) => (
  <div
    onClick={onSelect}
    className='flex flex-col gap-6px rounded-9px p-9px transition-colors'
    style={{
      background: 'var(--nfe-inset-bg)',
      border: '1px solid ' + (selected ? 'var(--nfe-accent)' : 'var(--nfe-panel-border)'),
    }}
  >
    <div className='flex items-center justify-between'>
      <span className='text-12px font-600' style={{ color: 'var(--nfe-text-1)' }}>
        {label} · {Math.round((line.pos / maxPos) * 100)}%
      </span>
      <div
        role='button'
        tabIndex={0}
        title='delete'
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onDelete();
          }
        }}
        className='grid h-22px w-22px cursor-pointer place-items-center rounded-6px'
        style={{ color: 'var(--nfe-text-3)' }}
      >
        <Delete theme='outline' size={13} />
      </div>
    </div>
    <div className='flex items-center gap-8px'>
      <span className='text-11px' style={{ color: 'var(--nfe-text-3)' }}>
        {gapLabel}
      </span>
      <div className='flex-1'>
        <Slider min={0} max={MAX_GAP} value={line.gap} onChange={(v) => onGap(v as number)} />
      </div>
      <span className='w-38px text-right text-11px tabular-nums' style={{ color: 'var(--nfe-text-2)' }}>
        {line.gap} px
      </span>
    </div>
  </div>
);

// ─── Overlay renderer ─────────────────────────────────────────────────────────

const SplitOverlay: React.FC<{
  vp: Viewport;
  naturalWidth: number;
  naturalHeight: number;
  xDividers: Divider[];
  yDividers: Divider[];
  lines: CustomLine[];
  selected: number | null;
}> = ({ vp, naturalWidth, naturalHeight, xDividers, yDividers, lines, selected }) => {
  const toX = (v: number) => v * vp.scale + vp.offsetX;
  const toY = (v: number) => v * vp.scale + vp.offsetY;
  const top = toY(0);
  const bottom = toY(naturalHeight);
  const left = toX(0);
  const right = toX(naturalWidth);
  const selectedLine = lines.find((l) => l.id === selected) ?? null;

  return (
    <div className='pointer-events-none absolute inset-0'>
      {/* Seam strips (removed regions) */}
      {xDividers.map((d, i) => (
        <div
          key={`sx${i}`}
          className='absolute'
          style={{ left: toX(d.pos - d.gap / 2), top, width: Math.max(0, d.gap * vp.scale), height: bottom - top, background: 'var(--nfe-seam)' }}
        />
      ))}
      {yDividers.map((d, i) => (
        <div
          key={`sy${i}`}
          className='absolute'
          style={{ left, top: toY(d.pos - d.gap / 2), width: right - left, height: Math.max(0, d.gap * vp.scale), background: 'var(--nfe-seam)' }}
        />
      ))}
      {/* Divider center lines */}
      {xDividers.map((d, i) => (
        <div key={`lx${i}`} className='absolute' style={{ left: toX(d.pos), top, width: 1, height: bottom - top, background: 'rgba(255,255,255,0.85)' }} />
      ))}
      {yDividers.map((d, i) => (
        <div key={`ly${i}`} className='absolute' style={{ left, top: toY(d.pos), width: right - left, height: 1, background: 'rgba(255,255,255,0.85)' }} />
      ))}
      {/* Selected custom line highlight */}
      {selectedLine &&
        (selectedLine.orient === 'v' ? (
          <div className='absolute' style={{ left: toX(selectedLine.pos) - 1, top, width: 3, height: bottom - top, background: 'var(--nfe-accent)' }} />
        ) : (
          <div className='absolute' style={{ left, top: toY(selectedLine.pos) - 1, width: right - left, height: 3, background: 'var(--nfe-accent)' }} />
        ))}
      {/* Image frame */}
      <div className='absolute' style={{ left, top, width: right - left, height: bottom - top, outline: '1px solid rgba(255,255,255,0.35)' }} />
    </div>
  );
};

export default SplitTool;
