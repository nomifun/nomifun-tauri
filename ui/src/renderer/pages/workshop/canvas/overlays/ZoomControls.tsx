/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * ZoomControls — the bottom-center floating zoom bar: zoom out / slider /
 * live percentage / zoom in, plus reset-to-100% and fit-to-content. The slider
 * is log-scaled so the wide 0.05–4× range stays usable.
 */

import React, { useCallback } from 'react';
import { useReactFlow, useStore } from '@xyflow/react';
import { FullScreen, ZoomIn, ZoomOut } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { FIT_VIEW_OPTIONS, ZOOM_MAX, ZOOM_MIN } from '../model';

const LOG_MIN = Math.log(ZOOM_MIN);
const LOG_MAX = Math.log(ZOOM_MAX);

function zoomToSlider(zoom: number): number {
  const clamped = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, zoom));
  return (Math.log(clamped) - LOG_MIN) / (LOG_MAX - LOG_MIN);
}
function sliderToZoom(t: number): number {
  return Math.exp(LOG_MIN + t * (LOG_MAX - LOG_MIN));
}

const CHROME_BTN =
  'grid h-28px w-28px place-items-center rounded-7px cursor-pointer transition-colors text-[var(--color-text-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]';

const ZoomControls: React.FC = () => {
  const { t } = useTranslation();
  const { zoomTo, fitView } = useReactFlow();
  const zoom = useStore((s) => s.transform[2]);

  const onSlider = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      zoomTo(sliderToZoom(Number(e.target.value)), { duration: 0 });
    },
    [zoomTo]
  );

  return (
    <div className='flex items-center gap-4px rounded-11px border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] px-6px py-4px shadow-[0_8px_28px_rgba(0,0,0,0.16)] backdrop-blur-md'>
      <div
        role='button'
        tabIndex={0}
        title={t('workshopCanvas.zoom.out', { defaultValue: '缩小' })}
        aria-label={t('workshopCanvas.zoom.out', { defaultValue: '缩小' })}
        onClick={() => zoomTo(Math.max(ZOOM_MIN, zoom / 1.25), { duration: 160 })}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            zoomTo(Math.max(ZOOM_MIN, zoom / 1.25), { duration: 160 });
          }
        }}
        className={CHROME_BTN}
      >
        <ZoomOut theme='outline' size={16} strokeWidth={3} />
      </div>

      <input
        type='range'
        min={0}
        max={1}
        step={0.001}
        value={zoomToSlider(zoom)}
        onChange={onSlider}
        aria-label={t('workshopCanvas.zoom.slider', { defaultValue: '缩放' })}
        className='nomi-ws-zoom-slider h-3px w-120px cursor-pointer'
      />

      <div
        role='button'
        tabIndex={0}
        title={t('workshopCanvas.zoom.in', { defaultValue: '放大' })}
        aria-label={t('workshopCanvas.zoom.in', { defaultValue: '放大' })}
        onClick={() => zoomTo(Math.min(ZOOM_MAX, zoom * 1.25), { duration: 160 })}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            zoomTo(Math.min(ZOOM_MAX, zoom * 1.25), { duration: 160 });
          }
        }}
        className={CHROME_BTN}
      >
        <ZoomIn theme='outline' size={16} strokeWidth={3} />
      </div>

      <div
        role='button'
        tabIndex={0}
        title={t('workshopCanvas.zoom.reset', { defaultValue: '重置为 100%' })}
        onClick={() => zoomTo(1, { duration: 200 })}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            zoomTo(1, { duration: 200 });
          }
        }}
        className='min-w-46px rounded-6px px-6px py-3px text-center text-12px font-600 tabular-nums text-[var(--color-text-2)] cursor-pointer hover:bg-[var(--color-fill-2)]'
      >
        {Math.round(zoom * 100)}%
      </div>

      <div
        role='button'
        tabIndex={0}
        title={t('workshopCanvas.zoom.fit', { defaultValue: '适应视图' })}
        aria-label={t('workshopCanvas.zoom.fit', { defaultValue: '适应视图' })}
        onClick={() => fitView(FIT_VIEW_OPTIONS)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            fitView(FIT_VIEW_OPTIONS);
          }
        }}
        className={CHROME_BTN}
      >
        <FullScreen theme='outline' size={16} strokeWidth={3} />
      </div>
    </div>
  );
};

export default ZoomControls;
