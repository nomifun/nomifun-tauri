/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import type { DragEvent } from 'react';
import type { TFunction } from 'i18next';
import { ipcBridge } from '@/common';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import { FileService } from '@/renderer/services/FileService';
import type { MessageApi } from '../types';

interface UseWorkspaceDragImportOptions {
  onFilesDropped: (files: Array<{ path: string; name: string }>) => Promise<void> | void;
  messageApi: MessageApi;
  t: TFunction<'translation'>;
  /** Stable upload-tracking identity (conversation id string for WebUI HTTP uploads). */
  sourceKey: string;
}

interface DroppedItem {
  path: string;
  name: string;
  kind: 'file' | 'directory';
}

const getBaseName = (targetPath: string): string => {
  const parts = targetPath.replace(/[\\/]+$/, '').split(/[\\/]/);
  return parts.pop() || targetPath;
};

const dedupeItems = (items: DroppedItem[]): DroppedItem[] => {
  const map = new Map<string, DroppedItem>();
  for (const item of items) {
    if (!map.has(item.path)) {
      map.set(item.path, item);
    }
  }
  return Array.from(map.values());
};

export function useWorkspaceDragImport({
  onFilesDropped,
  messageApi,
  t,
  sourceKey,
}: UseWorkspaceDragImportOptions) {
  const [isDragging, setIsDragging] = useState(false);
  const dragCounterRef = useRef(0);

  const resetDragState = useCallback(() => {
    dragCounterRef.current = 0;
    setIsDragging(false);
  }, []);

  const handleDragEnter = useCallback((event: DragEvent) => {
    event.preventDefault();
    event.stopPropagation();
    dragCounterRef.current += 1;
    setIsDragging(true);
  }, []);

  const handleDragOver = useCallback(
    (event: DragEvent) => {
      event.preventDefault();
      event.stopPropagation();
      if (!isDragging) {
        setIsDragging(true);
      }
    },
    [isDragging]
  );

  const handleDragLeave = useCallback((event: DragEvent) => {
    event.preventDefault();
    event.stopPropagation();
    dragCounterRef.current = Math.max(0, dragCounterRef.current - 1);
    if (dragCounterRef.current === 0) {
      setIsDragging(false);
    }
  }, []);

  const createTempItemsFromFiles = useCallback(
    async (files: File[]): Promise<DroppedItem[]> => {
      if (!files.length) return [];
      const pseudoList = Object.assign([...files], {
        length: files.length,
        item: (index: number) => files[index] || null,
      }) as unknown as FileList;

      const processed = await FileService.processDroppedFiles(pseudoList, sourceKey, 'workspace');
      return processed.map((meta) => ({ path: meta.path, name: meta.name, kind: 'file' as const }));
    },
    [sourceKey]
  );

  /**
   * 解析拖拽的项目，检测是文件还是目录
   * Resolve dropped items, detect whether they are files or directories
   */
  const resolveDroppedItems = useCallback(async (items: DroppedItem[]): Promise<DroppedItem[]> => {
    const unique = new Map<string, DroppedItem>();

    for (const item of items) {
      try {
        const metadata = await ipcBridge.fs.getFileMetadata.invoke({ path: item.path });
        const itemName = metadata.name || item.name || getBaseName(item.path);
        const kind = metadata.isDirectory ? 'directory' : 'file';
        unique.set(item.path, { path: item.path, name: itemName, kind });
      } catch (error) {
        console.warn('[WorkspaceDragImport] Failed to inspect dropped path:', item.path, error);
        const fallbackName = item.name || getBaseName(item.path);
        unique.set(item.path, { path: item.path, name: fallbackName, kind: 'file' });
      }
    }

    return Array.from(unique.values());
  }, []);

  /**
   * 派发解析后的拖拽项目到上层回调
   * Dispatch resolved dropped items to the upper-layer callback.
   */
  const dispatchTargets = useCallback(
    async (targets: DroppedItem[]) => {
      if (targets.length === 0) {
        messageApi.warning(
          t('conversation.workspace.dragNoFiles', {
            defaultValue: 'No valid files detected. Please drag from Finder/Explorer.',
          })
        );
        return;
      }

      try {
        await onFilesDropped(targets.map(({ path, name }) => ({ path, name })));
      } catch (error) {
        console.error('Failed to import dropped files:', error);
        messageApi.error(
          t('conversation.workspace.dragFailed', {
            defaultValue: 'Failed to import dropped files.',
          })
        );
      }
    },
    [messageApi, onFilesDropped, t]
  );

  /**
   * 处理一组绝对路径（Tauri 原生 drop 或其他能拿到绝对路径的来源）
   * Process a batch of absolute host paths (Tauri native drop or any other source
   * that can resolve a real file_path) through the same downstream as the legacy
   * "file with absolute path" branch.
   */
  const handleAbsolutePaths = useCallback(
    async (paths: string[]) => {
      if (!paths.length) return;
      const seeds: DroppedItem[] = paths.map((p) => ({ path: p, name: getBaseName(p), kind: 'file' }));
      const deduped = dedupeItems(seeds);
      const resolved = await resolveDroppedItems(deduped);
      await dispatchTargets(resolved);
    },
    [dispatchTargets, resolveDroppedItems]
  );

  const handleDrop = useCallback(
    async (event: DragEvent) => {
      event.preventDefault();
      event.stopPropagation();
      resetDragState();

      // Under Tauri, native file drops are delivered via onDragDropEvent (see useEffect
      // below) with absolute paths. The browser drop handler only sees File objects
      // without host paths, so we route them through the WebUI/HTTP-upload fallback.
      const dataTransfer = event.dataTransfer || event.nativeEvent?.dataTransfer;
      const filesWithoutPath: File[] = [];

      if (dataTransfer?.files && dataTransfer.files.length > 0) {
        for (let i = 0; i < dataTransfer.files.length; i++) {
          const file = dataTransfer.files[i];
          const item = dataTransfer.items?.[i];
          const entry = item?.webkitGetAsEntry?.();
          if (entry?.isDirectory) {
            // Directory without an absolute path cannot be processed via the WebUI fallback.
            console.warn('[WorkspaceDragImport] Directory without path property, cannot process:', entry.name);
          } else {
            filesWithoutPath.push(file);
          }
        }
      }

      let tempItems: DroppedItem[] = [];
      if (filesWithoutPath.length > 0) {
        try {
          tempItems = await createTempItemsFromFiles(filesWithoutPath);
        } catch (error) {
          console.error('[WorkspaceDragImport] Failed to create temp files:', error);
        }
      }

      await dispatchTargets(tempItems);
    },
    [createTempItemsFromFiles, dispatchTargets, resetDragState]
  );

  /**
   * Tauri v2: subscribe to native drag-drop events so we receive real absolute
   * host paths (HTML5 File drops in the webview don't expose them). Web/PWA
   * builds skip this entirely — the dynamic import keeps Tauri code out of the
   * browser bundle.
   */
  useEffect(() => {
    if (!isTauriRuntime()) return;

    let unlisten: (() => void) | undefined;
    let cancelled = false;

    void (async () => {
      try {
        const { getCurrentWebview } = await import('@tauri-apps/api/webview');
        const fn = await getCurrentWebview().onDragDropEvent((event) => {
          const payload = event.payload as { type: string; paths?: string[] };
          if (payload?.type === 'drop' && Array.isArray(payload.paths) && payload.paths.length > 0) {
            resetDragState();
            void handleAbsolutePaths(payload.paths);
          }
        });
        if (cancelled) {
          fn();
        } else {
          unlisten = fn;
        }
      } catch (error) {
        console.warn('[WorkspaceDragImport] Failed to attach Tauri drag-drop listener:', error);
      }
    })();

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [handleAbsolutePaths, resetDragState]);

  const dragHandlers = {
    onDragEnter: handleDragEnter,
    onDragOver: handleDragOver,
    onDragLeave: handleDragLeave,
    onDrop: handleDrop,
  };

  return {
    isDragging,
    dragHandlers,
  };
}
