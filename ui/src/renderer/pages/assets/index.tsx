/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * AssetLibraryPage (`/assets`) — the platform-level 资产库 (Asset Library).
 *
 * A unified management surface for the creative-workshop assets that were
 * previously reachable only through the in-canvas drawer. It reuses the shared
 * data controller (`useAssetLibrary`), cards, and detail/edit/create modals,
 * and adds management-grade capabilities the drawer lacks: sorting, tag
 * filtering, multi-select bulk operations (move / tag / remove / download /
 * delete), collection rename, per-asset download, and prev/next in the detail
 * sheet. Visual language mirrors `WorkshopListPage` / the knowledge pages
 * (centered max-w shell, gradient-badge header, auto-fill card grid).
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Button, Input, Modal, Result, Select, Spin } from '@arco-design/web-react';
import { useTranslation } from 'react-i18next';
import { CheckOne, Close, Delete, Download, FileText, FolderClose, ImageFiles, Search, SortTwo, Tag, Upload } from '@icon-park/react';

import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';

import type { AssetSortKey, PatchAssetBody, WorkshopAsset } from '../workshop/types';
import { deleteAsset as apiDeleteAsset, patchAsset as apiPatchAsset, renameCollection as apiRenameCollection } from '../workshop/api';
import { revokeWorkshopMedia } from '../workshop/lib/media';
import {
  COLLECTION_ALL,
  COLLECTION_UNGROUPED,
  useAssetLibrary,
  type AssetKindFilter,
} from '../workshop/assets/useAssetLibrary';
import AssetCard from '../workshop/assets/AssetCard';
import AssetDetailModal from '../workshop/assets/AssetDetailModal';
import AssetEditModal from '../workshop/assets/AssetEditModal';
import CreateTextAssetModal from '../workshop/assets/CreateTextAssetModal';
import { downloadAsset } from './download';

// ─── Segmented kind filter (mirrors the drawer's control) ─────────────────────

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
            'select-none rounded-7px px-12px py-4px text-center text-12px font-500 cursor-pointer transition-all duration-120',
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

// ─── Upload tray (compact, page variant) ──────────────────────────────────────

const UploadTray: React.FC<{
  uploads: ReturnType<typeof useAssetLibrary>['uploads'];
  onCancel: (localId: string) => void;
  onClearDone: () => void;
  t: ReturnType<typeof useTranslation>['t'];
}> = ({ uploads, onCancel, onClearDone, t }) => {
  if (uploads.length === 0) return null;
  const hasFinished = uploads.some((u) => u.status !== 'uploading');
  return (
    <div className='flex flex-col gap-8px rounded-12px border border-solid border-[var(--color-border-2)] bg-[var(--color-fill-1)] px-16px py-12px'>
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
        <div key={u.localId} className='flex items-center gap-10px'>
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

// ─── Bulk action bar ──────────────────────────────────────────────────────────

interface BulkBarProps {
  count: number;
  t: ReturnType<typeof useTranslation>['t'];
  onMove: () => void;
  onTag: () => void;
  onDownload: () => void;
  onRemove: () => void;
  onDelete: () => void;
  onClear: () => void;
}

const BulkBar: React.FC<BulkBarProps> = ({ count, t, onMove, onTag, onDownload, onRemove, onDelete, onClear }) => {
  const actions: { key: string; icon: React.ReactNode; label: string; run: () => void; danger?: boolean }[] = [
    { key: 'move', icon: <FolderClose theme='outline' size={14} strokeWidth={3} />, label: t('assetLibrary.bulk.moveToCollection', { defaultValue: '移到集合' }), run: onMove },
    { key: 'tag', icon: <Tag theme='outline' size={14} strokeWidth={3} />, label: t('assetLibrary.bulk.addTags', { defaultValue: '加标签' }), run: onTag },
    { key: 'download', icon: <Download theme='outline' size={14} strokeWidth={3} />, label: t('assetLibrary.bulk.download', { defaultValue: '下载' }), run: onDownload },
    { key: 'remove', icon: <Close theme='outline' size={14} strokeWidth={3} />, label: t('assetLibrary.bulk.removeFromLibrary', { defaultValue: '移出库' }), run: onRemove },
    { key: 'delete', icon: <Delete theme='outline' size={14} strokeWidth={3} />, label: t('assetLibrary.bulk.delete', { defaultValue: '删除' }), run: onDelete, danger: true },
  ];
  return (
    <div className='flex flex-wrap items-center gap-x-14px gap-y-8px rounded-12px border border-solid border-[rgba(var(--primary-6),0.35)] bg-[rgba(var(--primary-6),0.06)] px-14px py-10px'>
      <span className='text-13px font-600 text-[var(--color-text-1)]'>
        {t('assetLibrary.selection.count', { count, defaultValue: '已选 {{count}} 项' })}
      </span>
      <div className='flex flex-wrap items-center gap-6px'>
        {actions.map((a) => (
          <div
            key={a.key}
            role='button'
            tabIndex={0}
            onClick={a.run}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                a.run();
              }
            }}
            className={[
              'inline-flex items-center gap-5px rounded-8px border border-solid px-10px py-5px text-12px font-500 cursor-pointer transition-colors',
              a.danger
                ? 'border-[rgba(var(--danger-6),0.35)] text-[rgb(var(--danger-6))] bg-[var(--color-bg-2)] hover:bg-[rgba(var(--danger-6),0.08)]'
                : 'border-[var(--color-border-2)] text-[var(--color-text-2)] bg-[var(--color-bg-2)] hover:bg-[var(--color-fill-2)] hover:text-[var(--color-text-1)]',
            ].join(' ')}
          >
            {a.icon}
            {a.label}
          </div>
        ))}
      </div>
      <div
        role='button'
        tabIndex={0}
        onClick={onClear}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onClear();
          }
        }}
        className='ml-auto text-12px text-[var(--color-text-3)] cursor-pointer hover:text-[var(--color-text-1)]'
      >
        {t('assetLibrary.selection.clear', { defaultValue: '取消选择' })}
      </div>
    </div>
  );
};

// ─── Page ─────────────────────────────────────────────────────────────────────

const SORT_OPTIONS: { value: AssetSortKey; labelKey: string; fallback: string }[] = [
  { value: 'created_desc', labelKey: 'assetLibrary.sort.createdDesc', fallback: '最新' },
  { value: 'created_asc', labelKey: 'assetLibrary.sort.createdAsc', fallback: '最早' },
  { value: 'updated_desc', labelKey: 'assetLibrary.sort.updatedDesc', fallback: '最近更新' },
  { value: 'name_asc', labelKey: 'assetLibrary.sort.nameAsc', fallback: '名称' },
  { value: 'size_desc', labelKey: 'assetLibrary.sort.sizeDesc', fallback: '大小' },
];

const AssetLibraryPage: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [message, holder] = useArcoMessage();
  const lib = useAssetLibrary(true);

  const [detailAsset, setDetailAsset] = useState<WorkshopAsset | null>(null);
  const [editAsset, setEditAsset] = useState<WorkshopAsset | null>(null);
  const [creatingText, setCreatingText] = useState(false);

  const [selected, setSelected] = useState<Set<string>>(new Set());
  const selectionActive = selected.size > 0;

  // Bulk-op modals.
  const [moveOpen, setMoveOpen] = useState(false);
  const [moveTarget, setMoveTarget] = useState<string | undefined>(undefined);
  const [moveSaving, setMoveSaving] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);
  const [tagValues, setTagValues] = useState<string[]>([]);
  const [tagSaving, setTagSaving] = useState(false);
  const [renameOpen, setRenameOpen] = useState(false);
  const [renameTo, setRenameTo] = useState('');
  const [renameSaving, setRenameSaving] = useState(false);

  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const dragDepth = useRef(0);

  // ─── Seen-tags accumulation (grow-only) for the tag filter row ───────────────
  const [seenTags, setSeenTags] = useState<string[]>([]);
  useEffect(() => {
    setSeenTags((prev) => {
      const set = new Set(prev);
      let changed = false;
      for (const a of lib.items) {
        for (const tg of a.tags) {
          if (!set.has(tg)) {
            set.add(tg);
            changed = true;
          }
        }
      }
      if (lib.tag && !set.has(lib.tag)) {
        set.add(lib.tag);
        changed = true;
      }
      return changed ? [...set].sort((a, b) => a.localeCompare(b)) : prev;
    });
  }, [lib.items, lib.tag]);

  // Clear selection whenever the result set is re-scoped (filters/sort change),
  // so bulk actions never operate on now-hidden items.
  useEffect(() => {
    setSelected(new Set());
  }, [lib.kind, lib.collection, lib.tag, lib.sort, lib.query]);

  // Prune selection to ids still present. A single-card delete / edit-out-of-
  // library removes an item without touching the selection set, so without this
  // the bulk count and confirm dialogs would overstate the affected items.
  useEffect(() => {
    setSelected((prev) => {
      if (prev.size === 0) return prev;
      const present = new Set(lib.items.map((a) => a.id));
      let changed = false;
      const next = new Set<string>();
      for (const id of prev) {
        if (present.has(id)) next.add(id);
        else changed = true;
      }
      return changed ? next : prev;
    });
  }, [lib.items]);

  // ─── Upload wiring ───────────────────────────────────────────────────────────
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

  // ─── Infinite scroll ─────────────────────────────────────────────────────────
  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el || lib.loadingMore || !lib.hasMore) return;
    if (el.scrollTop + el.clientHeight >= el.scrollHeight - 320) lib.loadMore();
  }, [lib]);

  // ─── Selection helpers ───────────────────────────────────────────────────────
  const toggleSelect = useCallback((asset: WorkshopAsset) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(asset.id)) next.delete(asset.id);
      else next.add(asset.id);
      return next;
    });
  }, []);
  const selectAll = useCallback(() => {
    setSelected(new Set(lib.displayItems.map((a) => a.id)));
  }, [lib.displayItems]);
  const clearSelection = useCallback(() => setSelected(new Set()), []);

  // ─── Detail prev / next (within the loaded list) ─────────────────────────────
  const detailIndex = detailAsset ? lib.displayItems.findIndex((a) => a.id === detailAsset.id) : -1;
  const hasPrev = detailIndex > 0;
  const hasNext = detailIndex >= 0 && detailIndex < lib.displayItems.length - 1;
  const goPrev = useCallback(() => {
    if (detailIndex > 0) setDetailAsset(lib.displayItems[detailIndex - 1]);
  }, [detailIndex, lib.displayItems]);
  const goNext = useCallback(() => {
    if (detailIndex >= 0 && detailIndex < lib.displayItems.length - 1) {
      setDetailAsset(lib.displayItems[detailIndex + 1]);
    }
  }, [detailIndex, lib.displayItems]);

  // ─── Single-asset actions ────────────────────────────────────────────────────
  const handleEdit = useCallback((asset: WorkshopAsset) => {
    setDetailAsset(null);
    setEditAsset(asset);
  }, []);

  const handleDownload = useCallback(
    async (asset: WorkshopAsset) => {
      try {
        await downloadAsset(asset);
      } catch (e) {
        message.error(
          `${t('assetLibrary.actionFailed', { defaultValue: '操作失败' })}: ${e instanceof Error ? e.message : String(e)}`
        );
      }
    },
    [message, t]
  );

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

  // ─── Bulk actions ────────────────────────────────────────────────────────────
  const bulkTargets = useCallback(
    () => lib.items.filter((a) => selected.has(a.id)),
    [lib.items, selected]
  );

  const applyBulkPatch = useCallback(
    async (patchFor: (a: WorkshopAsset) => PatchAssetBody): Promise<{ ok: number; fail: number }> => {
      let ok = 0;
      let fail = 0;
      for (const asset of bulkTargets()) {
        try {
          await apiPatchAsset(asset.id, patchFor(asset));
          ok += 1;
        } catch {
          fail += 1;
        }
      }
      return { ok, fail };
    },
    [bulkTargets]
  );

  const finishBulk = useCallback(
    (ok: number, fail: number, successMsg: string) => {
      lib.reload();
      setSelected(new Set());
      if (fail === 0) message.success(successMsg);
      else message.warning(t('assetLibrary.bulk.partial', { ok, fail, defaultValue: '{{ok}} 项成功，{{fail}} 项失败' }));
    },
    [lib, message, t]
  );

  const submitMove = useCallback(async () => {
    setMoveSaving(true);
    const target = moveTarget?.trim();
    const { ok, fail } = await applyBulkPatch(() => ({ collection: target ? target : null }));
    setMoveSaving(false);
    setMoveOpen(false);
    finishBulk(ok, fail, t('assetLibrary.bulk.doneMove', { count: ok, defaultValue: '已移动 {{count}} 项' }));
  }, [moveTarget, applyBulkPatch, finishBulk, t]);

  const submitTags = useCallback(async () => {
    const tags = tagValues.map((s) => s.trim()).filter(Boolean);
    if (tags.length === 0) {
      message.warning(t('assetLibrary.tagModal.empty', { defaultValue: '请至少输入一个标签' }));
      return;
    }
    setTagSaving(true);
    const { ok, fail } = await applyBulkPatch((a) => ({ tags: [...new Set([...a.tags, ...tags])] }));
    setTagSaving(false);
    setTagOpen(false);
    setTagValues([]);
    finishBulk(ok, fail, t('assetLibrary.bulk.doneTag', { count: ok, defaultValue: '已为 {{count}} 项添加标签' }));
  }, [tagValues, applyBulkPatch, finishBulk, message, t]);

  const bulkDownload = useCallback(async () => {
    const targets = bulkTargets();
    let ok = 0;
    let fail = 0;
    for (const asset of targets) {
      try {
        await downloadAsset(asset);
        ok += 1;
      } catch {
        fail += 1;
      }
    }
    if (fail === 0) {
      message.success(t('assetLibrary.bulk.downloadStarted', { count: ok, defaultValue: '已开始下载 {{count}} 项' }));
    } else {
      message.warning(t('assetLibrary.bulk.partial', { ok, fail, defaultValue: '{{ok}} 项成功，{{fail}} 项失败' }));
    }
  }, [bulkTargets, message, t]);

  const bulkRemove = useCallback(() => {
    const count = selected.size;
    Modal.confirm({
      title: t('assetLibrary.removeConfirm.title', { defaultValue: '移出资产库' }),
      content: t('assetLibrary.removeConfirm.content', {
        count,
        defaultValue: '选中的 {{count}} 项将移出资产库（仍保留在画布中，但可能被后续清理回收）。确定继续？',
      }),
      okText: t('assetLibrary.removeConfirm.ok', { defaultValue: '移出' }),
      cancelText: t('assetLibrary.cancel', { defaultValue: '取消' }),
      onOk: async () => {
        const { ok, fail } = await applyBulkPatch(() => ({ in_library: false }));
        finishBulk(ok, fail, t('assetLibrary.bulk.doneRemove', { count: ok, defaultValue: '已移出 {{count}} 项' }));
      },
    });
  }, [selected.size, applyBulkPatch, finishBulk, t]);

  const bulkDelete = useCallback(() => {
    const count = selected.size;
    Modal.confirm({
      title: t('assetLibrary.deleteConfirm.title', { defaultValue: '删除资产' }),
      content: t('assetLibrary.deleteConfirm.content', {
        count,
        defaultValue: '确定永久删除选中的 {{count}} 项？该操作不可撤销。',
      }),
      okText: t('assetLibrary.deleteConfirm.ok', { defaultValue: '删除' }),
      cancelText: t('assetLibrary.cancel', { defaultValue: '取消' }),
      okButtonProps: { status: 'danger' },
      onOk: async () => {
        let ok = 0;
        let fail = 0;
        for (const asset of bulkTargets()) {
          try {
            await apiDeleteAsset(asset.id);
            revokeWorkshopMedia(asset.id);
            ok += 1;
          } catch {
            fail += 1;
          }
        }
        finishBulk(ok, fail, t('assetLibrary.bulk.doneDelete', { count: ok, defaultValue: '已删除 {{count}} 项' }));
      },
    });
  }, [selected.size, bulkTargets, finishBulk, t]);

  // ─── Collection rename ───────────────────────────────────────────────────────
  const namedCollectionSelected =
    lib.collection !== COLLECTION_ALL && lib.collection !== COLLECTION_UNGROUPED ? lib.collection : null;

  const openRename = useCallback(() => {
    if (!namedCollectionSelected) return;
    setRenameTo(namedCollectionSelected);
    setRenameOpen(true);
  }, [namedCollectionSelected]);

  const submitRename = useCallback(async () => {
    if (!namedCollectionSelected) return;
    const to = renameTo.trim();
    if (!to) {
      message.warning(t('assetLibrary.renameCollection.required', { defaultValue: '请输入新的集合名' }));
      return;
    }
    // Renaming to the same name is a no-op — just close.
    if (to === namedCollectionSelected) {
      setRenameOpen(false);
      return;
    }
    setRenameSaving(true);
    try {
      const updated = await apiRenameCollection(namedCollectionSelected, to);
      setRenameOpen(false);
      lib.setCollection(to);
      lib.reload();
      message.success(t('assetLibrary.renameCollection.done', { count: updated, defaultValue: '已更新 {{count}} 项' }));
    } catch (e) {
      message.error(
        `${t('assetLibrary.actionFailed', { defaultValue: '操作失败' })}: ${e instanceof Error ? e.message : String(e)}`
      );
    } finally {
      setRenameSaving(false);
    }
  }, [namedCollectionSelected, renameTo, lib, message, t]);

  // ─── Derived UI bits ─────────────────────────────────────────────────────────
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

  const collectionOptions = useMemo(
    () => [
      { label: t('workshopAssets.collection.all', { defaultValue: '全部集合' }), value: COLLECTION_ALL },
      { label: t('workshopAssets.collection.ungrouped', { defaultValue: '未分组' }), value: COLLECTION_UNGROUPED },
      ...lib.collections.map((c) => ({ label: c, value: c })),
    ],
    [lib.collections, t]
  );

  const sortOptions = useMemo(
    () => SORT_OPTIONS.map((o) => ({ label: t(o.labelKey, { defaultValue: o.fallback }), value: o.value })),
    [t]
  );

  const showOnboarding = !lib.isFiltering;

  // ─── Render ──────────────────────────────────────────────────────────────────
  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
      className={[
        'relative size-full box-border overflow-y-auto',
        isMobile ? 'px-16px py-14px' : 'px-12px py-24px md:px-40px md:py-32px',
      ].join(' ')}
    >
      {holder}
      <div className='mx-auto flex w-full max-w-1180px box-border flex-col gap-16px'>
        {/* Header */}
        <div className='flex w-full flex-wrap items-start justify-between gap-x-20px gap-y-12px'>
          <div className='flex items-start gap-12px min-w-0'>
            <span
              className='flex items-center justify-center w-40px h-40px rd-11px shrink-0 text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.26) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <ImageFiles theme='outline' size='22' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='min-w-0'>
              <h1 className='m-0 mb-3px text-22px font-bold text-[var(--color-text-1)] tracking-tight'>
                {t('assetLibrary.title', { defaultValue: '资产库' })}
              </h1>
              <p className='m-0 text-13px text-[var(--color-text-3)] leading-19px max-w-560px'>
                {t('assetLibrary.subtitle', {
                  defaultValue: '统一管理你在创意工坊中沉淀的图片、视频与文本素材：上传、分组、打标签、批量整理与复用。',
                })}
              </p>
            </div>
          </div>

          <div className='flex items-center gap-10px'>
            <Button type='primary' onClick={openFilePicker}>
              <span className='inline-flex items-center gap-6px'>
                <Upload theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                {t('workshopAssets.upload.button', { defaultValue: '上传' })}
              </span>
            </Button>
            <Button onClick={() => setCreatingText(true)}>
              <span className='inline-flex items-center gap-6px'>
                <FileText theme='outline' size='15' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
                {t('workshopAssets.newText.button', { defaultValue: '文本' })}
              </span>
            </Button>
          </div>
        </div>

        <input ref={fileInputRef} type='file' accept='image/*,video/*' multiple hidden onChange={onFileInputChange} />

        {/* Controls */}
        <div className='flex flex-wrap items-center gap-10px'>
          <div className='flex min-w-200px flex-1 items-center gap-8px rounded-10px border border-solid border-[var(--color-border-3)] bg-[var(--color-fill-2)] px-12px py-8px'>
            <Search theme='outline' size={14} className='shrink-0 text-[var(--color-text-3)]' />
            <input
              className='w-full border-none bg-transparent font-[inherit] text-13px text-[var(--color-text-1)] outline-none placeholder:text-[var(--color-text-3)]'
              placeholder={t('workshopAssets.search.placeholder', { defaultValue: '搜索资产...' })}
              value={lib.query}
              onChange={(e) => lib.setQuery(e.target.value)}
            />
          </div>
          <SegmentedKindFilter value={lib.kind} onChange={lib.setKind} labelOf={kindLabel} />
          <div className='flex items-center gap-6px'>
            <Select
              showSearch
              allowCreate
              value={lib.collection}
              onChange={(v) => lib.setCollection(v as string)}
              options={collectionOptions}
              className='w-160px'
            />
            {namedCollectionSelected && (
              <div
                role='button'
                tabIndex={0}
                title={t('assetLibrary.renameCollection.trigger', { defaultValue: '重命名集合' })}
                onClick={openRename}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    openRename();
                  }
                }}
                className='text-12px text-[var(--color-text-3)] cursor-pointer whitespace-nowrap hover:text-[rgb(var(--primary-6))]'
              >
                {t('assetLibrary.renameCollection.trigger', { defaultValue: '重命名集合' })}
              </div>
            )}
          </div>
          <div className='flex items-center gap-6px'>
            <SortTwo theme='outline' size={15} className='text-[var(--color-text-3)]' />
            <Select
              value={lib.sort}
              onChange={(v) => lib.setSort(v as AssetSortKey)}
              options={sortOptions}
              className='w-128px'
            />
          </div>
        </div>

        {/* Tag filter row */}
        {seenTags.length > 0 && (
          <div className='flex flex-wrap items-center gap-6px'>
            <span className='mr-2px inline-flex items-center gap-4px text-12px text-[var(--color-text-3)]'>
              <Tag theme='outline' size={13} strokeWidth={3} />
              {t('assetLibrary.tagFilter.label', { defaultValue: '标签' })}
            </span>
            {seenTags.map((tg) => {
              const active = lib.tag === tg;
              return (
                <div
                  key={tg}
                  role='button'
                  tabIndex={0}
                  onClick={() => lib.setTag(active ? null : tg)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      lib.setTag(active ? null : tg);
                    }
                  }}
                  className={[
                    'inline-flex items-center gap-4px rounded-full border border-solid px-10px py-3px text-12px cursor-pointer transition-colors',
                    active
                      ? '!bg-primary-1 !text-primary-6 border-[var(--color-primary-light-3)] font-medium'
                      : 'bg-[var(--color-fill-2)] text-[var(--color-text-2)] border-[var(--color-border-2)] hover:border-[var(--color-border-3)] hover:text-[var(--color-text-1)]',
                  ].join(' ')}
                >
                  {tg}
                  {active && <Close theme='outline' size={11} strokeWidth={4} />}
                </div>
              );
            })}
          </div>
        )}

        {/* Selection / bulk bar */}
        {selectionActive ? (
          <BulkBar
            count={selected.size}
            t={t}
            onMove={() => {
              setMoveTarget(undefined);
              setMoveOpen(true);
            }}
            onTag={() => {
              setTagValues([]);
              setTagOpen(true);
            }}
            onDownload={() => void bulkDownload()}
            onRemove={bulkRemove}
            onDelete={bulkDelete}
            onClear={clearSelection}
          />
        ) : (
          !lib.loading &&
          !lib.error &&
          lib.displayItems.length > 0 && (
            <div className='flex items-center justify-between text-12px text-[var(--color-text-3)]'>
              <span>{t('workshopAssets.count.total', { count: lib.total, defaultValue: '共 {{count}} 项' })}</span>
              <div
                role='button'
                tabIndex={0}
                onClick={selectAll}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    selectAll();
                  }
                }}
                className='inline-flex items-center gap-4px cursor-pointer hover:text-[var(--color-text-1)]'
              >
                <CheckOne theme='outline' size={13} strokeWidth={3} />
                {t('assetLibrary.selection.selectAll', { defaultValue: '全选本页' })}
              </div>
            </div>
          )
        )}

        {/* Upload tray */}
        <UploadTray uploads={lib.uploads} onCancel={lib.cancelUpload} onClearDone={lib.clearFinishedUploads} t={t} />

        {/* Body */}
        {lib.loading ? (
          <div className='flex justify-center py-56px'>
            <Spin />
          </div>
        ) : lib.error ? (
          <Result
            status='error'
            title={t('workshopAssets.loadError', { defaultValue: '加载资产失败' })}
            subTitle={lib.error}
            extra={<Button onClick={lib.reload}>{t('workshopAssets.retry', { defaultValue: '重试' })}</Button>}
          />
        ) : lib.displayItems.length === 0 ? (
          <div className='flex flex-col items-center gap-14px rd-16px border border-dashed border-[var(--color-border-2)] bg-fill-1 px-20px py-52px text-center'>
            <span
              className='flex items-center justify-center w-56px h-56px rd-16px text-[rgb(var(--primary-6))]'
              style={{
                background: 'linear-gradient(150deg, rgba(var(--primary-5),0.16) 0%, rgba(var(--primary-6),0.28) 100%)',
                border: '1px solid rgba(var(--primary-6),0.22)',
              }}
            >
              <ImageFiles theme='outline' size='28' fill='currentColor' className='block' style={{ lineHeight: 0 }} />
            </span>
            <div className='flex flex-col gap-4px'>
              <span className='text-15px font-600 text-[var(--color-text-1)]'>
                {showOnboarding
                  ? t('workshopAssets.empty.title', { defaultValue: '资产库还是空的' })
                  : t('workshopAssets.empty.filterTitle', { defaultValue: '没有匹配的资产' })}
              </span>
              <span className='text-13px text-[var(--color-text-3)] max-w-[440px]'>
                {showOnboarding
                  ? t('workshopAssets.empty.desc', {
                      defaultValue: '上传图片、视频，或新建文本资产，开始积累你的创作素材。',
                    })
                  : t('workshopAssets.empty.filterDesc', { defaultValue: '换个关键词或筛选条件再试试。' })}
              </span>
            </div>
            {showOnboarding ? (
              <div className='flex items-center gap-8px'>
                <Button type='primary' onClick={openFilePicker}>
                  {t('workshopAssets.empty.uploadCta', { defaultValue: '上传素材' })}
                </Button>
                <Button onClick={() => setCreatingText(true)}>
                  {t('workshopAssets.empty.newTextCta', { defaultValue: '新建文本' })}
                </Button>
              </div>
            ) : (
              <Button onClick={lib.clearFilters}>{t('workshopAssets.empty.clearFilters', { defaultValue: '清除筛选' })}</Button>
            )}
          </div>
        ) : (
          <>
            <div
              className='grid gap-14px'
              style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(min(240px, 100%), 1fr))' }}
            >
              {lib.displayItems.map((asset) => (
                <AssetCard
                  key={asset.id}
                  asset={asset}
                  t={t}
                  draggable={false}
                  selectable
                  selected={selected.has(asset.id)}
                  selectionActive={selectionActive}
                  onToggleSelect={toggleSelect}
                  onOpenDetail={setDetailAsset}
                  onEdit={handleEdit}
                  onDelete={handleDelete}
                  onDownload={(a) => void handleDownload(a)}
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
      </div>

      {/* Drag-to-upload overlay */}
      {dragActive && (
        <div className='pointer-events-none fixed inset-16px z-40 grid place-items-center rounded-16px border-2 border-dashed border-[rgb(var(--primary-6))] bg-[rgba(var(--primary-6),0.08)] backdrop-blur-sm'>
          <div className='flex flex-col items-center gap-8px text-center text-[rgb(var(--primary-6))]'>
            <Upload theme='outline' size={34} strokeWidth={3} />
            <span className='text-16px font-700'>{t('workshopAssets.upload.dropTitle', { defaultValue: '松开以上传' })}</span>
            <span className='max-w-[260px] text-13px text-[var(--color-text-2)]'>
              {t('workshopAssets.upload.dropDesc', { defaultValue: '把图片或视频拖到这里加入资产库' })}
            </span>
          </div>
        </div>
      )}

      {/* Detail / edit / create modals */}
      <AssetDetailModal
        asset={detailAsset}
        onClose={() => setDetailAsset(null)}
        onEdit={handleEdit}
        onDelete={handleDelete}
        onDownload={(a) => void handleDownload(a)}
        onPrev={goPrev}
        onNext={goNext}
        hasPrev={hasPrev}
        hasNext={hasNext}
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

      {/* Bulk: move to collection */}
      <Modal
        title={t('assetLibrary.moveModal.title', { defaultValue: '移动到集合' })}
        visible={moveOpen}
        confirmLoading={moveSaving}
        onOk={() => void submitMove()}
        onCancel={() => setMoveOpen(false)}
        okText={t('assetLibrary.moveModal.ok', { defaultValue: '移动' })}
        cancelText={t('assetLibrary.cancel', { defaultValue: '取消' })}
        autoFocus={false}
        unmountOnExit
      >
        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('assetLibrary.moveModal.label', { defaultValue: '目标集合' })}
          </span>
          <Select
            allowClear
            allowCreate
            showSearch
            value={moveTarget}
            onChange={(v) => setMoveTarget(v as string | undefined)}
            options={lib.collections.map((c) => ({ label: c, value: c }))}
            placeholder={t('assetLibrary.moveModal.placeholder', {
              defaultValue: '选择或输入集合名，留空则移出分组',
            })}
          />
        </label>
      </Modal>

      {/* Bulk: add tags */}
      <Modal
        title={t('assetLibrary.tagModal.title', { defaultValue: '批量添加标签' })}
        visible={tagOpen}
        confirmLoading={tagSaving}
        onOk={() => void submitTags()}
        onCancel={() => setTagOpen(false)}
        okText={t('assetLibrary.tagModal.ok', { defaultValue: '添加' })}
        cancelText={t('assetLibrary.cancel', { defaultValue: '取消' })}
        autoFocus={false}
        unmountOnExit
      >
        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('assetLibrary.tagModal.label', { defaultValue: '标签' })}
          </span>
          <Select
            mode='multiple'
            allowCreate
            allowClear
            value={tagValues}
            onChange={(v) => setTagValues(v as string[])}
            options={seenTags.map((tg) => ({ label: tg, value: tg }))}
            placeholder={t('assetLibrary.tagModal.placeholder', { defaultValue: '输入标签后回车' })}
          />
        </label>
      </Modal>

      {/* Bulk: rename collection */}
      <Modal
        title={t('assetLibrary.renameCollection.title', { defaultValue: '重命名集合' })}
        visible={renameOpen}
        confirmLoading={renameSaving}
        onOk={() => void submitRename()}
        onCancel={() => setRenameOpen(false)}
        okText={t('assetLibrary.renameCollection.ok', { defaultValue: '重命名' })}
        cancelText={t('assetLibrary.cancel', { defaultValue: '取消' })}
        autoFocus={false}
        unmountOnExit
      >
        <label className='flex flex-col gap-6px'>
          <span className='text-13px font-500 text-[var(--color-text-1)]'>
            {t('assetLibrary.renameCollection.label', { defaultValue: '新集合名' })}
          </span>
          <Input
            value={renameTo}
            onChange={setRenameTo}
            maxLength={60}
            placeholder={t('assetLibrary.renameCollection.placeholder', { defaultValue: '输入新的集合名' })}
            onPressEnter={() => void submitRename()}
          />
        </label>
      </Modal>
    </div>
  );
};

export default AssetLibraryPage;
