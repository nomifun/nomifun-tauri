/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Upscale tool — local (interpolation) enlargement to a target longest edge. */
import React, { forwardRef, useCallback, useEffect, useImperativeHandle, useState } from 'react';
import { useTranslation } from 'react-i18next';
import EditorStage, { type Viewport } from '../EditorStage';
import { PanelHint, PanelSection, StatPill, WorkArea } from '../PanelKit';
import { computeUpscaleTarget, estimatePngBytes, exportUpscale, formatBytes, type UpscaleAlgo } from '../lib/exporters';
import type { ImageToolHandle, ImageToolProps } from '../toolTypes';

const TARGETS = [1024, 2048, 4096] as const;
type Target = (typeof TARGETS)[number];

const UpscaleTool = forwardRef<ImageToolHandle, ImageToolProps>(({ image, onCanApplyChange }, ref) => {
  const { t } = useTranslation();
  const { el, naturalWidth: W, naturalHeight: H } = image;
  const [target, setTarget] = useState<Target>(2048);
  const [algo, setAlgo] = useState<UpscaleAlgo>('progressive');

  const dims = computeUpscaleTarget(W, H, target);
  const estBytes = estimatePngBytes(dims.width, dims.height);

  useEffect(() => {
    onCanApplyChange(true);
  }, [onCanApplyChange]);

  useImperativeHandle(ref, () => ({
    apply: async () => {
      const blob = await exportUpscale(el, W, H, { width: dims.width, height: dims.height }, algo);
      return { type: 'upscale', blob };
    },
  }));

  const algos: { value: UpscaleAlgo; title: string; desc: string }[] = [
    {
      value: 'progressive',
      title: t('workshopEditor.upscale.progressive', { defaultValue: '高清渐进' }),
      desc: t('workshopEditor.upscale.progressiveDesc', { defaultValue: '多次 2 倍逐步放大，边缘更平滑' }),
    },
    {
      value: 'bilinear',
      title: t('workshopEditor.upscale.bilinear', { defaultValue: '双线性' }),
      desc: t('workshopEditor.upscale.bilinearDesc', { defaultValue: '一次性平滑插值，速度快' }),
    },
    {
      value: 'nearest',
      title: t('workshopEditor.upscale.nearest', { defaultValue: '最近邻' }),
      desc: t('workshopEditor.upscale.nearestDesc', { defaultValue: '保留硬边像素，适合像素图' }),
    },
  ];

  const renderScreenLayer = useCallback(
    (_vp: Viewport) => (
      <div className='absolute left-1/2 top-14px -translate-x-1/2'>
        <div
          className='rounded-8px px-11px py-6px text-12px font-600 tabular-nums'
          style={{ background: 'var(--nfe-toolbar-bg)', color: 'var(--nfe-stage-text)' }}
        >
          {W} × {H} → {dims.width} × {dims.height}
        </div>
      </div>
    ),
    [W, H, dims.width, dims.height]
  );

  return (
    <WorkArea
      stage={<EditorStage src={el.src} naturalWidth={W} naturalHeight={H} renderScreenLayer={renderScreenLayer} />}
      panel={
        <>
          <PanelSection title={t('workshopEditor.upscale.target', { defaultValue: '目标尺寸（最长边）' })}>
            <div className='grid grid-cols-3 gap-6px'>
              {TARGETS.map((v) => {
                const activeT = v === target;
                return (
                  <div
                    key={v}
                    role='button'
                    tabIndex={0}
                    onClick={() => setTarget(v)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        setTarget(v);
                      }
                    }}
                    className='flex h-34px cursor-pointer items-center justify-center rounded-8px text-13px tabular-nums transition-all'
                    style={{
                      border: '1px solid ' + (activeT ? 'var(--nfe-accent)' : 'var(--nfe-panel-border)'),
                      background: activeT ? 'var(--nfe-accent-soft)' : 'var(--nfe-inset-bg)',
                      color: activeT ? 'var(--nfe-accent)' : 'var(--nfe-text-2)',
                      fontWeight: activeT ? 600 : 400,
                    }}
                  >
                    {v === 1024 ? '1K' : v === 2048 ? '2K' : '4K'}
                  </div>
                );
              })}
            </div>
          </PanelSection>

          <PanelSection title={t('workshopEditor.upscale.algo', { defaultValue: '放大算法' })}>
            <div className='flex flex-col gap-6px'>
              {algos.map((a) => {
                const activeA = a.value === algo;
                return (
                  <div
                    key={a.value}
                    role='button'
                    tabIndex={0}
                    onClick={() => setAlgo(a.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        setAlgo(a.value);
                      }
                    }}
                    className='flex cursor-pointer flex-col gap-2px rounded-9px px-11px py-9px transition-all'
                    style={{
                      border: '1px solid ' + (activeA ? 'var(--nfe-accent)' : 'var(--nfe-panel-border)'),
                      background: activeA ? 'var(--nfe-accent-soft)' : 'var(--nfe-inset-bg)',
                    }}
                  >
                    <span className='text-13px font-600' style={{ color: activeA ? 'var(--nfe-accent)' : 'var(--nfe-text-1)' }}>
                      {a.title}
                    </span>
                    <span className='text-11px leading-15px' style={{ color: 'var(--nfe-text-3)' }}>
                      {a.desc}
                    </span>
                  </div>
                );
              })}
            </div>
          </PanelSection>

          <PanelSection title={t('workshopEditor.upscale.summary', { defaultValue: '尺寸对比' })}>
            <div className='grid grid-cols-2 gap-8px'>
              <StatPill label={t('workshopEditor.upscale.before', { defaultValue: '原始' })} value={`${W}×${H}`} />
              <StatPill label={t('workshopEditor.upscale.after', { defaultValue: '放大后' })} value={`${dims.width}×${dims.height}`} />
            </div>
            <div className='flex items-center justify-between text-12px'>
              <span style={{ color: 'var(--nfe-text-3)' }}>{t('workshopEditor.upscale.estSize', { defaultValue: '预计大小' })}</span>
              <span className='tabular-nums font-600' style={{ color: 'var(--nfe-text-2)' }}>
                ~ {formatBytes(estBytes)}
              </span>
            </div>
          </PanelSection>

          <PanelHint>{t('workshopEditor.upscale.hint', { defaultValue: '本地插值放大不会新增细节，仅提升分辨率；需要更清晰的结果请使用 AI 超分。' })}</PanelHint>
        </>
      }
    />
  );
});

UpscaleTool.displayName = 'UpscaleTool';

export default UpscaleTool;
