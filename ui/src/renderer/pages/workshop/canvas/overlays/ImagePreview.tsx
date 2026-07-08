/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * ImagePreview — a full-surface lightbox for image nodes: wheel-zoom,
 * drag-to-pan when zoomed, and prev/next paging across every image asset on the
 * canvas. Dismissed on Escape or backdrop click.
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { CloseSmall, DownloadOne, Left, Right, ZoomIn, ZoomOut } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useWorkshopMedia } from '../media';

const MIN = 0.2;
const MAX = 8;

export interface ImagePreviewProps {
  assetIds: string[];
  startIndex: number;
  onClose: () => void;
  onDownload: (assetId: string) => void;
}

const ImagePreview: React.FC<ImagePreviewProps> = ({ assetIds, startIndex, onClose, onDownload }) => {
  const { t } = useTranslation();
  const [index, setIndex] = useState(() => Math.max(0, Math.min(startIndex, assetIds.length - 1)));
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const dragRef = useRef<{ x: number; y: number; px: number; py: number } | null>(null);

  const total = assetIds.length;
  const assetId = assetIds[index] ?? null;
  const media = useWorkshopMedia(assetId);

  const reset = useCallback(() => {
    setZoom(1);
    setPan({ x: 0, y: 0 });
  }, []);

  const step = useCallback(
    (delta: number) => {
      if (total <= 1) return;
      setIndex((i) => (i + delta + total) % total);
      reset();
    },
    [total, reset]
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') onClose();
      else if (e.key === 'ArrowLeft') step(-1);
      else if (e.key === 'ArrowRight') step(1);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose, step]);

  const onWheel = (e: React.WheelEvent): void => {
    const next = Math.max(MIN, Math.min(MAX, zoom * (e.deltaY < 0 ? 1.12 : 0.89)));
    setZoom(next);
    if (next === 1) setPan({ x: 0, y: 0 });
  };

  const onPointerDown = (e: React.PointerEvent): void => {
    if (zoom <= 1) return;
    dragRef.current = { x: e.clientX, y: e.clientY, px: pan.x, py: pan.y };
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
  };
  const onPointerMove = (e: React.PointerEvent): void => {
    const d = dragRef.current;
    if (!d) return;
    setPan({ x: d.px + (e.clientX - d.x), y: d.py + (e.clientY - d.y) });
  };
  const onPointerUp = (): void => {
    dragRef.current = null;
  };

  const NavBtn: React.FC<{ label: string; onClick: () => void; children: React.ReactNode; side: 'l' | 'r' }> = ({
    label,
    onClick,
    children,
    side,
  }) => (
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
          onClick();
        }
      }}
      className={[
        'absolute top-1/2 grid h-44px w-44px -translate-y-1/2 place-items-center rounded-full cursor-pointer',
        'bg-[rgba(0,0,0,0.4)] text-white transition-colors hover:bg-[rgba(0,0,0,0.62)]',
        side === 'l' ? 'left-20px' : 'right-20px',
      ].join(' ')}
    >
      {children}
    </div>
  );

  return (
    <div
      className='absolute inset-0 z-50 flex items-center justify-center bg-[rgba(0,0,0,0.82)]'
      onClick={onClose}
      onWheel={onWheel}
    >
      {/* Top bar */}
      <div className='absolute left-0 right-0 top-0 flex items-center gap-10px px-18px py-14px' onClick={(e) => e.stopPropagation()}>
        {total > 1 && (
          <span className='rounded-full bg-[rgba(0,0,0,0.4)] px-10px py-4px text-12px font-600 tabular-nums text-white'>
            {index + 1} / {total}
          </span>
        )}
        <div className='ml-auto flex items-center gap-8px'>
          <div
            role='button'
            tabIndex={0}
            title={t('workshopCanvas.preview.zoomOut', { defaultValue: '缩小' })}
            onClick={(e) => {
              e.stopPropagation();
              setZoom((z) => Math.max(MIN, z * 0.83));
            }}
            className='grid h-34px w-34px place-items-center rounded-8px bg-[rgba(255,255,255,0.12)] text-white cursor-pointer hover:bg-[rgba(255,255,255,0.22)]'
          >
            <ZoomOut theme='outline' size={17} strokeWidth={3} />
          </div>
          <div
            role='button'
            tabIndex={0}
            title={t('workshopCanvas.preview.zoomIn', { defaultValue: '放大' })}
            onClick={(e) => {
              e.stopPropagation();
              setZoom((z) => Math.min(MAX, z * 1.2));
            }}
            className='grid h-34px w-34px place-items-center rounded-8px bg-[rgba(255,255,255,0.12)] text-white cursor-pointer hover:bg-[rgba(255,255,255,0.22)]'
          >
            <ZoomIn theme='outline' size={17} strokeWidth={3} />
          </div>
          {assetId && (
            <div
              role='button'
              tabIndex={0}
              title={t('workshopCanvas.preview.download', { defaultValue: '下载' })}
              onClick={(e) => {
                e.stopPropagation();
                onDownload(assetId);
              }}
              className='grid h-34px w-34px place-items-center rounded-8px bg-[rgba(255,255,255,0.12)] text-white cursor-pointer hover:bg-[rgba(255,255,255,0.22)]'
            >
              <DownloadOne theme='outline' size={17} strokeWidth={3} />
            </div>
          )}
          <div
            role='button'
            tabIndex={0}
            title={t('workshopCanvas.preview.close', { defaultValue: '关闭' })}
            onClick={onClose}
            className='grid h-34px w-34px place-items-center rounded-8px bg-[rgba(255,255,255,0.12)] text-white cursor-pointer hover:bg-[rgba(255,255,255,0.22)]'
          >
            <CloseSmall theme='outline' size={22} strokeWidth={3} />
          </div>
        </div>
      </div>

      {total > 1 && (
        <>
          <NavBtn label={t('workshopCanvas.preview.prev', { defaultValue: '上一张' })} onClick={() => step(-1)} side='l'>
            <Left theme='outline' size={22} strokeWidth={3} />
          </NavBtn>
          <NavBtn label={t('workshopCanvas.preview.next', { defaultValue: '下一张' })} onClick={() => step(1)} side='r'>
            <Right theme='outline' size={22} strokeWidth={3} />
          </NavBtn>
        </>
      )}

      {media.status === 'ready' ? (
        <img
          src={media.url}
          alt=''
          draggable={false}
          onClick={(e) => e.stopPropagation()}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          className='max-h-[86%] max-w-[86%] select-none object-contain'
          style={{
            transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})`,
            cursor: zoom > 1 ? 'grab' : 'default',
            transition: dragRef.current ? 'none' : 'transform 0.12s ease-out',
          }}
        />
      ) : media.status === 'error' ? (
        <span className='text-14px text-white/80'>{t('workshopCanvas.preview.loadFailed', { defaultValue: '图片加载失败' })}</span>
      ) : (
        <span className='h-28px w-28px animate-spin rounded-full border-2 border-solid border-white/30 border-t-white' />
      )}
    </div>
  );
};

export default ImagePreview;
