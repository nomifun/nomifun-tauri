/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Asset library panel (M4 module).
 *
 * A right-docked slide-in drawer (~360px) the canvas page mounts and toggles
 * with the `A` shortcut / a toolbar button. It expects a positioned ancestor
 * (the canvas root is `relative`) since it pins to that box with `absolute`.
 *
 * Capabilities (PRD §2.4 asset library, P0 + P1 asset parts):
 *  - search (debounced) + kind + collection filters
 *  - upload via button and full-panel file drag-and-drop, with a progress /
 *    cancel tray
 *  - masonry-ish equal grid of image / video / text cards; hover actions
 *    (insert into canvas, edit, delete); click → detail sheet with provenance
 *  - new text asset authoring
 *  - drag a card onto the canvas (writeAssetDrag; M1 receives it)
 *  - infinite scroll pagination (page_size 40) + empty / filter-empty states
 *  - Esc closes the panel (captured so it doesn't reach the canvas)
 *
 * `AssetsPanelProps` is the frozen M0 contract between the canvas (M1) and the
 * asset library (M4).
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Modal, Select, Spin } from '@arco-design/web-react';
import { Close, Delete, FileText, Platte, Search, Upload } from '@icon-park/react';

import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import type { WorkshopAsset } from '../types';
import {
  COLLECTION_ALL,
  COLLECTION_UNGROUPED,
  useAssetLibrary,
  type AssetKindFilter,
} from './useAssetLibrary';
import AssetCard from './AssetCard';
import AssetDetailModal from './AssetDetailModal';
import AssetEditModal from './AssetEditModal';
import CreateTextAssetModal from './CreateTextAssetModal';

export interface AssetsPanelProps {
  /** Canvas the panel is opened from (used to scope "insert" actions). */
  canvasId: string;
  open: boolean;
  onClose: () => void;
  /** Called when the user picks "insert into canvas" on an asset. */
  onInsertAsset: (asset: WorkshopAsset) => void;
}

// ─── Segmented kind filter ────────────────────────────────────────────────────

const KIND_SEGMENTS: AssetKindFilter[] = ['all', 'image', 'video', 'text'];

const SegmentedKindFilter: React.FC<{
  value: AssetKindFilter;
  onChange: (v: AssetKindFilter) => void;
  labelOf: (k: AssetKindFilter) => string;
}> = ({ value, onChange, labelOf }) => (
  <div className='flex items-center gap-2px rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] p-2px'>
    {KIND_SEGMENTS.map((k) => {
      const active = value === k;
      return (
        <div
          key={k}
          role='button'
          tabIndex={0}
          onClick={() => onChange(k)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onChange(k);
            }
          }}
          className={[
            'flex-1 select-none rounded-7px px-2px py-4px text-center text-12px font-500 cursor-pointer transition-all duration-120',
            active
              ? 'bg-[var(--color-bg-2)] text-[var(--color-text-1)] shadow-[0_1px_4px_rgba(0,0,0,0.1)]'
              : 'text-[var(--color-text-3)] hover:text-[var(--color-text-1)]',
          ].join(' ')}
        >
          {labelOf(k)}
        </div>
      );
    })}
  </div>
);

// ─── Upload tray ──────────────────────────────────────────────────────────────

const UploadTray: React.FC<{
  uploads: ReturnType<typeof useAssetLibrary>['uploads'];
  onCancel: (localId: string) => void;
  onClearDone: () => void;
  t: ReturnType<typeof useTranslation>['t'];
}> = ({ uploads, onCancel, onClearDone, t }) => {
  if (uploads.length === 0) return null;
  const hasFinished = uploads.some((u) => u.status !== 'uploading');
  return (
    <div className='flex flex-col gap-6px border-b border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-14px py-10px'>
      <div className='flex items-center justify-between'>
        <span className='text-11px font-600 uppercase tracking-wide text-[var(--color-text-4)]'>
          {t('workshopAssets.upload.queue', { defaultValue: '上传队列' })}
        </span>
        {hasFinished && (
          <div
            role='button'
            tabIndex={0}
            onClick={onClearDone}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onClearDone();
              }
            }}
            className='text-11px text-[var(--color-text-3)] cursor-pointer hover:text-[var(--color-text-1)]'
          >
            {t('workshopAssets.upload.clearDone', { defaultValue: '清除已完成' })}
          </div>
        )}
      </div>
      {uploads.map((u) => (
        <div key={u.localId} className='flex items-center gap-8px'>
          <div className='min-w-0 flex-1'>
            <div className='flex items-center justify-between gap-8px'>
              <span className='truncate text-12px text-[var(--color-text-2)]'>{u.fileName}</span>
              <span
                className={[
                  'shrink-0 text-11px font-600',
                  u.status === 'error' ? 'text-[rgb(var(--danger-6))]' : 'text-[var(--color-text-3)]',
                ].join(' ')}
              >
                {u.status === 'error'
                  ? t(`workshopAssets.upload.${u.error ?? 'failed'}`, { defaultValue: '上传失败' })
                  : `${u.percent}%`}
              </span>
            </div>
            <div className='mt-4px h-4px w-full overflow-hidden rounded-full bg-[var(--color-fill-3)]'>
              <div
                className={[
                  'h-full rounded-full transition-all duration-200',
                  u.status === 'error' ? 'bg-[rgb(var(--danger-6))]' : 'bg-[rgb(var(--primary-6))]',
                ].join(' ')}
                style={{ width: `${u.status === 'error' ? 100 : u.percent}%` }}
              />
            </div>
          </div>
          <div
            role='button'
            tabIndex={0}
            title={t('workshopAssets.upload.cancel', { defaultValue: '取消上传' })}
            onClick={() => onCancel(u.localId)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onCancel(u.localId);
              }
            }}
            className='grid h-22px w-22px shrink-0 place-items-center rounded-6px text-[var(--color-text-3)] cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]'
          >
            <Close theme='outline' size={13} strokeWidth={3} />
          </div>
        </div>
      ))}
    </div>
  );
};

// ─── Panel ──────────────────────────────────────────────────────────────────

const AssetsPanel: React.FC<AssetsPanelProps> = ({ open, onClose, onInsertAsset }) => {
  const { t } = useTranslation();
  const [message, holder] = useArcoMessage();
  const lib = useAssetLibrary(open);

  const [detailAsset, setDetailAsset] = useState<WorkshopAsset | null>(null);
  const [editAsset, setEditAsset] = useState<WorkshopAsset | null>(null);
  const [creatingText, setCreatingText] = useState(false);

  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const dragDepth = useRef(0);

  // Keep mounted through the slide-out transition, then unmount (stops fetches).
  const [mounted, setMounted] = useState(open);
  useEffect(() => {
    if (open) {
      setMounted(true);
      return;
    }
    const h = window.setTimeout(() => setMounted(false), 260);
    return () => window.clearTimeout(h);
  }, [open]);

  const anyModalOpen = detailAsset !== null || editAsset !== null || creatingText;

  // Esc closes the panel — captured so the canvas's own Esc handler never sees
  // it. Skipped while a child modal is open (Arco handles that Esc itself).
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || anyModalOpen) return;
      e.stopPropagation();
      e.preventDefault();
      onClose();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [open, anyModalOpen, onClose]);

  // ─── Upload wiring ──────────────────────────────────────────────────────────
  const openFilePicker = useCallback(() => fileInputRef.current?.click(), []);

  const onFileInputChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const files = Array.from(e.target.files ?? []);
      if (files.length) lib.startUploads(files);
      e.target.value = '';
    },
    [lib]
  );

  const isFileDrag = (e: React.DragEvent) => Array.from(e.dataTransfer.types).includes('Files');

  const onDragEnter = useCallback((e: React.DragEvent) => {
    if (!isFileDrag(e)) return;
    dragDepth.current += 1;
    setDragActive(true);
  }, []);

  const onDragOver = useCallback((e: React.DragEvent) => {
    if (!isFileDrag(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  }, []);

  const onDragLeave = useCallback((e: React.DragEvent) => {
    if (!isFileDrag(e)) return;
    dragDepth.current -= 1;
    if (dragDepth.current <= 0) {
      dragDepth.current = 0;
      setDragActive(false);
    }
  }, []);

  const onDrop = useCallback(
    (e: React.DragEvent) => {
      if (!isFileDrag(e)) return;
      e.preventDefault();
      dragDepth.current = 0;
      setDragActive(false);
      const files = Array.from(e.dataTransfer.files);
      if (files.length) lib.startUploads(files);
    },
    [lib]
  );

  // ─── Infinite scroll ────────────────────────────────────────────────────────
  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el || lib.loadingMore || !lib.hasMore) return;
    if (el.scrollTop + el.clientHeight >= el.scrollHeight - 260) lib.loadMore();
  }, [lib]);

  // ─── Actions ────────────────────────────────────────────────────────────────
  const handleInsert = useCallback(
    (asset: WorkshopAsset) => {
      onInsertAsset(asset);
      setDetailAsset(null);
      message.success(t('workshopAssets.insertOk', { defaultValue: '已插入画布' }));
    },
    [onInsertAsset, message, t]
  );

  const handleEdit = useCallback((asset: WorkshopAsset) => {
    setDetailAsset(null);
    setEditAsset(asset);
  }, []);

  const handleDelete = useCallback(
    (asset: WorkshopAsset) => {
      Modal.confirm({
        title: t('workshopAssets.delete.confirmTitle', { defaultValue: '删除资产' }),
        content: t('workshopAssets.delete.confirmContent', {
          title: asset.title,
          defaultValue: '确定删除「{{title}}」吗？该文件将被永久删除，无法撤销。',
        }),
        okText: t('workshopAssets.delete.ok', { defaultValue: '删除' }),
        cancelText: t('workshopAssets.delete.cancel', { defaultValue: '取消' }),
        okButtonProps: { status: 'danger' },
        onOk: async () => {
          try {
            await lib.remove(asset.id);
            setDetailAsset((cur) => (cur?.id === asset.id ? null : cur));
            message.success(t('workshopAssets.delete.done', { defaultValue: '资产已删除' }));
          } catch (e) {
            message.error(
              `${t('workshopAssets.delete.failed', { defaultValue: '删除失败' })}: ${e instanceof Error ? e.message : String(e)}`
            );
          }
        },
      });
    },
    [lib, message, t]
  );

  const kindLabel = useCallback(
    (k: AssetKindFilter): string => {
      const map: Record<AssetKindFilter, string> = {
        all: t('workshopAssets.kind.all', { defaultValue: '全部' }),
        image: t('workshopAssets.kind.image', { defaultValue: '图片' }),
        video: t('workshopAssets.kind.video', { defaultValue: '视频' }),
        text: t('workshopAssets.kind.text', { defaultValue: '文本' }),
      };
      return map[k];
    },
    [t]
  );

  const collectionOptions = [
    { label: t('workshopAssets.collection.all', { defaultValue: '全部集合' }), value: COLLECTION_ALL },
    { label: t('workshopAssets.collection.ungrouped', { defaultValue: '未分组' }), value: COLLECTION_UNGROUPED },
    ...lib.collections.map((c) => ({ label: c, value: c })),
  ];

  const displayCount = lib.collection === COLLECTION_UNGROUPED ? lib.displayItems.length : lib.total;
  const showOnboarding = !lib.isFiltering;

  if (!mounted) return null;

  return (
    <div
      className={[
        'absolute inset-y-0 right-0 z-30 flex w-360px max-w-[calc(100vw-24px)] flex-col',
        'border-l border-solid border-[var(--color-border-2)] bg-[var(--color-bg-2)]',
        'shadow-[-16px_0_44px_rgba(0,0,0,0.16)]',
        'transition-transform duration-260 ease-out',
        open ? 'translate-x-0' : 'translate-x-full',
      ].join(' ')}
      onKeyDown={(e) => e.stopPropagation()}
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      {holder}

      {/* Header */}
      <div className='shrink-0 flex items-center gap-10px border-b border-solid border-[var(--color-border-2)] px-14px py-12px'>
        <span
          className='grid h-28px w-28px shrink-0 place-items-center rounded-8px text-[rgb(var(--primary-6))]'
          style={{ background: 'rgba(var(--primary-6),0.12)' }}
        >
          <Platte theme='outline' size={16} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
        </span>
        <div className='min-w-0 flex-1'>
          <div className='truncate text-14px font-700 leading-[1.2] text-[var(--color-text-1)]'>
            {t('workshopAssets.title', { defaultValue: '资产库' })}
          </div>
          {displayCount > 0 && (
            <div className='text-11px text-[var(--color-text-3)]'>
              {t('workshopAssets.count.total', { count: displayCount, defaultValue: '共 {{count}} 项' })}
            </div>
          )}
        </div>
        <div
          role='button'
          tabIndex={0}
          title={t('workshopAssets.close', { defaultValue: '关闭' })}
          onClick={onClose}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onClose();
            }
          }}
          className='grid h-30px w-30px shrink-0 place-items-center rounded-8px text-[var(--color-text-3)] cursor-pointer hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)] transition-colors'
        >
          <Close theme='outline' size={17} strokeWidth={3} />
        </div>
      </div>

      {/* Toolbar */}
      <div className='shrink-0 flex flex-col gap-10px border-b border-solid border-[var(--color-border-2)] px-14px py-12px'>
        <div className='flex items-center gap-8px rounded-9px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-10px py-7px'>
          <Search theme='outline' size={14} className='shrink-0 text-[var(--color-text-3)]' />
          <input
            className='w-full border-none bg-transparent font-[inherit] text-13px text-[var(--color-text-1)] outline-none placeholder:text-[var(--color-text-3)]'
            placeholder={t('workshopAssets.search.placeholder', { defaultValue: '搜索资产...' })}
            value={lib.query}
            onChange={(e) => lib.setQuery(e.target.value)}
          />
        </div>

        <SegmentedKindFilter value={lib.kind} onChange={lib.setKind} labelOf={kindLabel} />

        <div className='flex items-center gap-8px'>
          <div className='min-w-0 flex-1'>
            <Select
              size='small'
              showSearch
              allowCreate
              value={lib.collection}
              onChange={(v) => lib.setCollection(v as string)}
              options={collectionOptions}
              className='w-full'
            />
          </div>
          <Button
            type='primary'
            size='small'
            onClick={openFilePicker}
          >
            <span className='inline-flex items-center gap-5px'>
              <Upload theme='outline' size={14} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
              {t('workshopAssets.upload.button', { defaultValue: '上传' })}
            </span>
          </Button>
          <Button
            size='small'
            onClick={() => setCreatingText(true)}
          >
            <span className='inline-flex items-center gap-5px'>
              <FileText theme='outline' size={14} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
              {t('workshopAssets.newText.button', { defaultValue: '文本' })}
            </span>
          </Button>
        </div>
      </div>

      <input
        ref={fileInputRef}
        type='file'
        accept='image/*,video/*'
        multiple
        hidden
        onChange={onFileInputChange}
      />

      <UploadTray uploads={lib.uploads} onCancel={lib.cancelUpload} onClearDone={lib.clearFinishedUploads} t={t} />

      {/* Body */}
      <div ref={scrollRef} onScroll={onScroll} className='relative min-h-0 flex-1 overflow-y-auto px-14px py-14px'>
        {lib.loading ? (
          <div className='grid h-full place-items-center'>
            <Spin />
          </div>
        ) : lib.error ? (
          <div className='flex flex-col items-center gap-10px py-40px text-center'>
            <span className='text-13px text-[var(--color-text-3)]'>
              {t('workshopAssets.loadError', { defaultValue: '加载资产失败' })}
            </span>
            <Button size='small' onClick={lib.reload}>
              {t('workshopAssets.retry', { defaultValue: '重试' })}
            </Button>
          </div>
        ) : lib.displayItems.length === 0 ? (
          <div className='flex h-full flex-col items-center justify-center gap-14px px-8px text-center'>
            <span
              className='grid h-56px w-56px place-items-center rounded-16px text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.28) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <Platte theme='outline' size={28} fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='flex flex-col gap-5px'>
              <span className='text-14px font-600 text-[var(--color-text-1)]'>
                {showOnboarding
                  ? t('workshopAssets.empty.title', { defaultValue: '资产库还是空的' })
                  : t('workshopAssets.empty.filterTitle', { defaultValue: '没有匹配的资产' })}
              </span>
              <span className='max-w-[260px] text-12px leading-[1.6] text-[var(--color-text-3)]'>
                {showOnboarding
                  ? t('workshopAssets.empty.desc', {
                      defaultValue: '上传图片、视频，或新建文本资产，开始积累你的创作素材。',
                    })
                  : t('workshopAssets.empty.filterDesc', { defaultValue: '换个关键词或筛选条件再试试。' })}
              </span>
            </div>
            {showOnboarding ? (
              <div className='flex items-center gap-8px'>
                <Button type='primary' size='small' onClick={openFilePicker}>
                  {t('workshopAssets.empty.uploadCta', { defaultValue: '上传素材' })}
                </Button>
                <Button size='small' onClick={() => setCreatingText(true)}>
                  {t('workshopAssets.empty.newTextCta', { defaultValue: '新建文本' })}
                </Button>
              </div>
            ) : (
              <Button size='small' onClick={lib.clearFilters}>
                {t('workshopAssets.empty.clearFilters', { defaultValue: '清除筛选' })}
              </Button>
            )}
          </div>
        ) : (
          <>
            <div className='grid grid-cols-2 gap-10px'>
              {lib.displayItems.map((asset) => (
                <AssetCard
                  key={asset.id}
                  asset={asset}
                  t={t}
                  onOpenDetail={setDetailAsset}
                  onInsert={handleInsert}
                  onEdit={handleEdit}
                  onDelete={handleDelete}
                />
              ))}
            </div>
            {lib.loadingMore && (
              <div className='flex items-center justify-center gap-8px py-14px text-12px text-[var(--color-text-3)]'>
                <Spin size={14} />
                {t('workshopAssets.count.loadingMore', { defaultValue: '加载中...' })}
              </div>
            )}
            {!lib.hasMore && lib.displayItems.length > 8 && (
              <div className='py-14px text-center text-11px text-[var(--color-text-4)]'>
                {t('workshopAssets.count.end', { defaultValue: '已经到底啦' })}
              </div>
            )}
          </>
        )}

        {/* Drag-to-upload overlay */}
        {dragActive && (
          <div className='pointer-events-none absolute inset-8px z-10 grid place-items-center rounded-14px border-2 border-dashed border-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.1)] backdrop-blur-sm'>
            <div className='flex flex-col items-center gap-8px text-center text-[rgb(var(--primary-6))]'>
              <Upload theme='outline' size={30} strokeWidth={3} />
              <span className='text-14px font-700'>{t('workshopAssets.upload.dropTitle', { defaultValue: '松开以上传' })}</span>
              <span className='max-w-[220px] text-12px text-[var(--color-text-2)]'>
                {t('workshopAssets.upload.dropDesc', { defaultValue: '把图片或视频拖到这里加入资产库' })}
              </span>
            </div>
          </div>
        )}
      </div>

      {/* Modals */}
      <AssetDetailModal
        asset={detailAsset}
        onClose={() => setDetailAsset(null)}
        onInsert={handleInsert}
        onEdit={handleEdit}
        onDelete={handleDelete}
      />
      <AssetEditModal
        asset={editAsset}
        collections={lib.collections}
        onClose={() => setEditAsset(null)}
        onSubmit={lib.patch}
      />
      <CreateTextAssetModal
        visible={creatingText}
        collections={lib.collections}
        onClose={() => setCreatingText(false)}
        onCreate={lib.createText}
      />
    </div>
  );
};

export default AssetsPanel;
