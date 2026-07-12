/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IDirOrFile } from '@/common/adapter/ipcBridge';
import { useEffect, useRef } from 'react';
import type { ContextMenuState, SelectedFile, WorkspaceSource } from '../types';

interface UseWorkspaceEventsOptions {
  /**
   * The active workspace source. Only its `key` (cache-reset identity) and the
   * optional `subscribe*`/`onSelectFiles` capabilities are used here — the body
   * stays source-agnostic.
   */
  source: WorkspaceSource;

  // Dependencies from useWorkspaceTree (body-owned tree state)
  refreshWorkspace: () => void;
  refreshChanges: () => Promise<void>;
  setFiles: React.Dispatch<React.SetStateAction<IDirOrFile[]>>;
  setSelected: React.Dispatch<React.SetStateAction<string[]>>;
  setExpandedKeys: React.Dispatch<React.SetStateAction<string[]>>;
  setTreeKey: React.Dispatch<React.SetStateAction<number>>;
  selectedNodeRef: React.MutableRefObject<{
    relativePath: string;
    fullPath: string;
  } | null>;
  selectedKeysRef: React.MutableRefObject<string[]>;

  // Dependencies from useWorkspaceModals
  closeContextMenu: () => void;
  setContextMenu: React.Dispatch<React.SetStateAction<ContextMenuState>>;
  closeRenameModal: () => void;
  closeDeleteModal: () => void;
}

/**
 * useWorkspaceEvents - 表面无关的事件接线
 * Source-agnostic event wiring for the workspace rail body.
 *
 * Owns only what every source shares:
 *  - cache reset when the source identity (`source.tree.key`) changes;
 *  - the pure-presentation context-menu outside-click / Escape close.
 *
 * Everything conversation-specific (agent-stream auto-refresh, manual-refresh,
 * SendBox selection sync, search-stream provider) is supplied by the source via
 * `subscribeRefresh` / `subscribeSelectionSync` / `subscribeFileTreeReplace`,
 * which this hook subscribes to generically.
 */
export function useWorkspaceEvents(options: UseWorkspaceEventsOptions) {
  const {
    source,
    refreshWorkspace,
    refreshChanges,
    setFiles,
    setSelected,
    setExpandedKeys,
    setTreeKey,
    selectedNodeRef,
    selectedKeysRef,
    closeContextMenu,
    setContextMenu,
    closeRenameModal,
    closeDeleteModal,
  } = options;

  const sourceKey = source.tree.key;

  // Keep latest callbacks/source in refs so the generic subscriptions below can
  // depend only on the stable `subscribe*` identities without re-subscribing
  // every render. Behavior matches the old direct listeners.
  const refreshRef = useRef(refreshWorkspace);
  refreshRef.current = refreshWorkspace;
  const refreshChangesRef = useRef(refreshChanges);
  refreshChangesRef.current = refreshChanges;
  const onSelectFilesRef = useRef(source.onSelectFiles);
  onSelectFilesRef.current = source.onSelectFiles;

  /**
   * 监听对话/源切换 - 重置所有状态
   * Reset all state when the source identity changes (conversation switch, or a
   * different terminal cwd). Mirrors the original conversation-reset effect.
   */
  useEffect(() => {
    setFiles([]);
    setSelected([]);
    setExpandedKeys([]);
    selectedNodeRef.current = null;
    selectedKeysRef.current = [];
    setTreeKey(Math.random());
    setContextMenu({ visible: false, x: 0, y: 0, node: null });
    closeRenameModal();
    closeDeleteModal();
    refreshRef.current();
    onSelectFilesRef.current?.([]);
  }, [
    sourceKey,
    setFiles,
    setSelected,
    setExpandedKeys,
    setTreeKey,
    selectedNodeRef,
    selectedKeysRef,
    setContextMenu,
    closeRenameModal,
    closeDeleteModal,
  ]);

  /**
   * 外部刷新触发（Agent 写入/手动刷新/源自定义）
   * External refresh triggers, owned by the source. The source decides when to
   * fire (e.g. throttled agent-stream writes, manual-refresh events).
   */
  const subscribeRefresh = source.subscribeRefresh;
  useEffect(() => {
    if (!subscribeRefresh) return;
    return subscribeRefresh(() => {
      refreshRef.current();
      refreshChangesRef.current();
    });
  }, [subscribeRefresh]);

  /**
   * 外部选中同步（SendBox 关闭标签时回推选中文件夹）(#1083)
   * Inbound selection sync, owned by the source. Applies the source's
   * normalized folder selection to the body's own selection state — preserving
   * the original filter logic exactly (only folders with a relativePath become
   * tree-selected keys; the last folder becomes the active selectedNodeRef).
   */
  const subscribeSelectionSync = source.subscribeSelectionSync;
  useEffect(() => {
    if (!subscribeSelectionSync) return;
    return subscribeSelectionSync((folders: SelectedFile[]) => {
      const newKeys = folders.filter((item) => item.relativePath).map((item) => item.relativePath);
      setSelected(newKeys);
      selectedKeysRef.current = newKeys;

      if (folders.length > 0) {
        const lastFolder = folders[folders.length - 1];
        selectedNodeRef.current = lastFolder.relativePath
          ? {
              relativePath: lastFolder.relativePath,
              fullPath: lastFolder.fullPath ?? '',
            }
          : null;
      } else {
        selectedNodeRef.current = null;
      }
    });
  }, [subscribeSelectionSync, setSelected, selectedKeysRef, selectedNodeRef]);

  /**
   * 搜索流式响应（源把单个匹配根节点推进来）
   * Streamed search-match replacement, owned by the source.
   */
  const subscribeFileTreeReplace = source.subscribeFileTreeReplace;
  useEffect(() => {
    if (!subscribeFileTreeReplace) return;
    return subscribeFileTreeReplace((root: IDirOrFile) => {
      setFiles([root]);
    });
  }, [subscribeFileTreeReplace, setFiles]);

  /**
   * 监听右键菜单外部点击 - 关闭菜单（纯展示，与源无关）
   * Listen to clicks outside context menu - close menu (pure presentation).
   */
  useEffect(() => {
    const handleClose = () => {
      closeContextMenu();
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        closeContextMenu();
      }
    };
    window.addEventListener('click', handleClose);
    window.addEventListener('scroll', handleClose, true);
    window.addEventListener('keydown', handleKeyDown);
    return () => {
      window.removeEventListener('click', handleClose);
      window.removeEventListener('scroll', handleClose, true);
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [closeContextMenu]);
}
