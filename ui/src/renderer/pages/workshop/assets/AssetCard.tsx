/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * AssetCard — one item in the asset-library grid.
 *
 * - Image / video: thumbnail (or poster) with a duration/kind badge and a
 *   caption footer.
 * - Text: a compact snippet card showing the content preview.
 *
 * The whole card is draggable onto the canvas via the frozen `writeAssetDrag`
 * contract (M1 turns the payload into a node). Clicking opens the detail view;
 * hover reveals insert / edit / delete actions.
 */

import React, { useCallback, useState } from 'react';
import type { TFunction } from 'i18next';
import { Check, Delete, Download, EditTwo, FileText, LinkOne } from '@icon-park/react';

import type { WorkshopAsset } from '../types';
import { writeAssetDrag } from '../lib/dnd';
import AssetThumb from './AssetThumb';
import { formatDuration, originDurationSeconds } from './format';

export interface AssetCardProps {
  asset: WorkshopAsset;
  t: TFunction;
  onOpenDetail: (asset: WorkshopAsset) => void;
  /** Insert-into-canvas action. Omitted on the standalone Asset Library page. */
  onInsert?: (asset: WorkshopAsset) => void;
  onEdit: (asset: WorkshopAsset) => void;
  onDelete: (asset: WorkshopAsset) => void;
  /** Download action (Asset Library page). Omitted in the in-canvas drawer. */
  onDownload?: (asset: WorkshopAsset) => void;
  /** Enable drag-to-canvas (default true). The platform page has no drop zone. */
  draggable?: boolean;
  /** Multi-select mode: renders a hover/persistent checkbox. */
  selectable?: boolean;
  /** Whether this card is currently selected. */
  selected?: boolean;
  /**
   * True when ≥1 asset is selected anywhere. In that state a plain card click
   * toggles selection instead of opening the detail sheet (gallery pattern).
   */
  selectionActive?: boolean;
  onToggleSelect?: (asset: WorkshopAsset) => void;
}

interface HoverAction {
  key: string;
  icon: React.ReactNode;
  label: string;
  run: () => void;
  danger?: boolean;
}

const HoverActions: React.FC<{ actions: HoverAction[] }> = ({ actions }) => (
  <div
    className={[
      'absolute right-8px top-8px flex gap-5px',
      'pointer-events-none opacity-0 transition-opacity duration-150',
      'group-hover:pointer-events-auto group-hover:opacity-100',
      'group-focus-within:pointer-events-auto group-focus-within:opacity-100',
    ].join(' ')}
    onClick={(e) => e.stopPropagation()}
  >
    {actions.map((action) => (
      <div
        key={action.key}
        role='button'
        tabIndex={0}
        title={action.label}
        onClick={action.run}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            action.run();
          }
        }}
        className={[
          'grid h-26px w-26px place-items-center rounded-7px cursor-pointer',
          'border border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)] backdrop-blur-sm',
          'transition-colors',
          action.danger
            ? 'text-[var(--color-text-3)] hover:!border-[rgba(var(--danger-6),0.4)] hover:!text-[rgb(var(--danger-6))] hover:!bg-[rgba(var(--danger-6),0.08)]'
            : 'text-[var(--color-text-3)] hover:border-[var(--color-border-3)] hover:text-[var(--color-text-1)] hover:bg-[var(--color-fill-2)]',
        ].join(' ')}
      >
        {action.icon}
      </div>
    ))}
  </div>
);

const AssetCard: React.FC<AssetCardProps> = ({
  asset,
  t,
  onOpenDetail,
  onInsert,
  onEdit,
  onDelete,
  onDownload,
  draggable = true,
  selectable = false,
  selected = false,
  selectionActive = false,
  onToggleSelect,
}) => {
  const [dragging, setDragging] = useState(false);

  const handleDragStart = useCallback(
    (e: React.DragEvent) => {
      if (!draggable) return;
      writeAssetDrag(e.dataTransfer, {
        asset_id: asset.id,
        kind: asset.kind,
        title: asset.title,
        width: asset.width,
        height: asset.height,
      });
      setDragging(true);
    },
    [asset, draggable]
  );

  const toggleSelect = useCallback(() => onToggleSelect?.(asset), [onToggleSelect, asset]);

  // In an active multi-select session a plain click toggles selection; otherwise
  // it opens the detail sheet (standard gallery behavior).
  const handleActivate = useCallback(() => {
    if (selectable && selectionActive) toggleSelect();
    else onOpenDetail(asset);
  }, [selectable, selectionActive, toggleSelect, onOpenDetail, asset]);

  const actions: HoverAction[] = [
    ...(onInsert
      ? [
          {
            key: 'insert',
            icon: <LinkOne theme='outline' size={13} strokeWidth={3} />,
            label: t('workshopAssets.card.insert', { defaultValue: '插入画布' }),
            run: () => onInsert(asset),
          },
        ]
      : []),
    ...(onDownload
      ? [
          {
            key: 'download',
            icon: <Download theme='outline' size={13} strokeWidth={3} />,
            label: t('workshopAssets.card.download', { defaultValue: '下载' }),
            run: () => onDownload(asset),
          },
        ]
      : []),
    {
      key: 'edit',
      icon: <EditTwo theme='outline' size={13} strokeWidth={3} />,
      label: t('workshopAssets.card.edit', { defaultValue: '编辑' }),
      run: () => onEdit(asset),
    },
    {
      key: 'delete',
      icon: <Delete theme='outline' size={13} strokeWidth={3} />,
      label: t('workshopAssets.card.delete', { defaultValue: '删除' }),
      run: () => onDelete(asset),
      danger: true,
    },
  ];

  const selectBox = selectable ? (
    <div
      role='checkbox'
      aria-checked={selected}
      tabIndex={0}
      title={t('workshopAssets.card.select', { defaultValue: '选择' })}
      onClick={(e) => {
        e.stopPropagation();
        toggleSelect();
      }}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          e.stopPropagation();
          toggleSelect();
        }
      }}
      className={[
        'absolute left-8px top-8px z-10 grid h-22px w-22px place-items-center rounded-6px cursor-pointer border border-solid transition-all',
        selected
          ? 'border-transparent bg-[rgb(var(--primary-6))] text-white opacity-100'
          : 'border-[var(--color-border-2)] bg-[var(--color-bg-2)] text-transparent opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 hover:border-[var(--color-border-3)]',
      ].join(' ')}
    >
      <Check theme='outline' size={13} strokeWidth={4} />
    </div>
  ) : null;

  const isText = asset.kind === 'text';
  const durationLabel = asset.kind === 'video' ? formatDuration(originDurationSeconds(asset.origin?.params)) : null;

  return (
    <div
      role='button'
      tabIndex={0}
      draggable={draggable}
      onDragStart={handleDragStart}
      onDragEnd={() => setDragging(false)}
      onClick={handleActivate}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          handleActivate();
        }
      }}
      className={[
        'group relative flex flex-col overflow-hidden rounded-12px border border-solid cursor-pointer select-none',
        'border-[var(--color-border-2)] bg-[var(--color-bg-2)] box-border',
        'transition-all duration-150',
        'hover:border-[var(--color-border-3)] hover:shadow-[0_8px_24px_rgba(0,0,0,0.14)] hover:-translate-y-1px',
        selected ? 'ring-2 ring-[rgb(var(--primary-6))] !border-[rgb(var(--primary-6))]' : '',
        dragging ? 'opacity-45 ring-2 ring-[rgba(var(--primary-6),0.5)]' : '',
      ].join(' ')}
    >
      {selectBox}
      {isText ? (
        // ── Text snippet ────────────────────────────────────────────────────
        <div className='flex flex-col gap-8px p-12px'>
          <div className='flex items-center gap-6px text-[rgb(var(--primary-6))]'>
            <FileText theme='outline' size={14} strokeWidth={3} />
            <span className='truncate text-13px font-600 text-[var(--color-text-1)]'>{asset.title}</span>
          </div>
          <p
            className='m-0 min-h-52px text-12px leading-[1.55] text-[var(--color-text-3)]'
            style={{
              display: '-webkit-box',
              WebkitLineClamp: 4,
              WebkitBoxOrient: 'vertical',
              overflow: 'hidden',
              whiteSpace: 'pre-wrap',
            }}
          >
            {asset.text_content?.trim() || t('workshopAssets.card.textFallback', { defaultValue: '空文本' })}
          </p>
          <HoverActions actions={actions} />
        </div>
      ) : (
        // ── Image / video ───────────────────────────────────────────────────
        <>
          <div className='relative w-full' style={{ aspectRatio: '1 / 1' }}>
            <AssetThumb asset={asset} />
            {durationLabel && (
              <span className='absolute bottom-6px right-6px rounded-5px bg-[rgba(0,0,0,0.6)] px-6px py-1px text-10px font-600 leading-[1.4] text-white'>
                {durationLabel}
              </span>
            )}
            {asset.kind === 'video' && !durationLabel && (
              <span className='absolute bottom-6px right-6px rounded-5px bg-[rgba(0,0,0,0.6)] px-6px py-1px text-10px font-600 leading-[1.4] text-white'>
                {t('workshopAssets.card.video', { defaultValue: '视频' })}
              </span>
            )}
            <HoverActions actions={actions} />
          </div>
          <div className='flex items-center gap-6px px-10px py-8px'>
            <span className='truncate text-12px font-500 text-[var(--color-text-1)]'>{asset.title}</span>
          </div>
        </>
      )}
    </div>
  );
};

export default AssetCard;
