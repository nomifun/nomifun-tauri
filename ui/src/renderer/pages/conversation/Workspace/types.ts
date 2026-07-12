/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IDirOrFile } from '@/common/adapter/ipcBridge';
import type { Message } from '@arco-design/web-react';
import type { ReactNode } from 'react';

export type MessageApi = Required<ReturnType<typeof Message.useMessage>[0]>;

/**
 * Workspace 组件的 Props 定义
 * Props definition for Workspace component
 */
export interface WorkspaceProps {
  workspace: string;
  conversation_id: number;
  /**
   * Authoritative "is this an auto-provisioned temporary workspace" flag.
   * Sourced from `conversation.extra.is_temporary_workspace` on the API
   * response (backend derives it from the data_dir path on every read).
   * Renamed here to camelCase per the frontend prop convention.
   */
  isTemporaryWorkspace?: boolean;
  eventPrefix?: 'acp' | 'codex' | 'nomi' | 'openclaw-gateway' | 'nanobot' | 'remote';
  messageApi?: MessageApi;
  extraTabs?: WorkspaceExtraTab[];
}

/**
 * 右键菜单状态
 * Context menu state
 */
export interface ContextMenuState {
  visible: boolean;
  x: number;
  y: number;
  node: IDirOrFile | null;
}

/**
 * 重命名弹窗状态
 * Rename modal state
 */
export interface RenameModalState {
  visible: boolean;
  value: string;
  target: IDirOrFile | null;
}

/**
 * 删除确认弹窗状态
 * Delete confirmation modal state
 */
export interface DeleteModalState {
  visible: boolean;
  target: IDirOrFile | null;
  loading: boolean;
}

/**
 * 粘贴确认弹窗状态
 * Paste confirmation modal state
 */
export interface PasteConfirmState {
  visible: boolean;
  file_name: string;
  filesToPaste: Array<{ path: string; name: string }>;
  doNotAsk: boolean;
  targetFolder: string | null;
}

/**
 * 节点选择引用，用于跟踪最后选中的文件夹节点
 * Node selection reference for tracking the last selected folder node
 */
export interface SelectedNodeRef {
  relativePath: string;
  fullPath: string;
}

/**
 * A file/folder selection emitted by the rail body when the user selects a node.
 * Source-agnostic: the body only knows it picked a node; the source decides what
 * to do with it (conversation → SendBox emitter, terminal → its own bridge).
 *
 * Shape mirrors the fields the conversation SendBox payload (`FileOrFolderItem`)
 * needs, so a conversation source can rebuild that payload without loss.
 */
export interface SelectedFile {
  /** Display name of the node. */
  name: string;
  /** Absolute host path. */
  fullPath: string;
  /** Path relative to the workspace root (empty string when unknown). */
  relativePath: string;
  /** True for files, false for folders. */
  isFile: boolean;
}

/**
 * Source-agnostic tree data provider. Drives lazy-loaded tree state in
 * `useWorkspaceTree` without any knowledge of conversations or terminals.
 *
 * - `key` is the cache-reset identity (conversation id string, or a cwd path).
 *   Changing it resets the tree and is the id carried on the has-files event.
 * - `listRoot` loads the top level (optionally filtered by `search`).
 * - `listChildren` lazily loads one node's direct children (the tree `loadMore`).
 */
export interface WorkspaceTreeSource {
  key: string;
  listRoot: (search?: string) => Promise<IDirOrFile[]>;
  listChildren: (node: { fullPath: string; relativePath: string }) => Promise<IDirOrFile[]>;
}

/**
 * Host-file import capability, supplied by sources that allow copying host
 * files into the workspace (conversation only today; terminal omits it).
 *
 * The actual paste/drag/upload handlers live in the body (they need the body's
 * live tree selection to resolve the paste-target folder). The source only
 * carries the surface-specific bits the generic upload hooks need — currently
 * just the upload-tracking identity. Its mere presence is what enables the
 * upload toolbar entry, drag overlay, and global paste capture.
 */
export interface WorkspaceUploadConfig {
  /**
   * Stable identity used to scope/track in-flight uploads (the conversation id
   * string for WebUI HTTP uploads + paste-service registration).
   */
  trackingKey: string;
}

export interface WorkspaceExtraTab {
  key: string;
  title: ReactNode;
  /** Optional icon rendered by the persistent vertical tool rail. */
  icon?: ReactNode;
  content: ReactNode;
}

/**
 * A pluggable workspace source. Feeds the presentational `WorkspaceRailBody`
 * with everything it needs, while keeping the body free of `if (terminal)`
 * branches. A conversation source and a (future) terminal source both implement
 * this; the body never imports either.
 */
export interface WorkspaceSource {
  /** Absolute workspace/cwd root. Used by file ops, preview, and file changes. */
  workspace: string;
  /** Tree data provider (lazy root + children). */
  tree: WorkspaceTreeSource;
  /** Whether this is an auto-provisioned temporary workspace (display hint). */
  isTemporary?: boolean;
  /**
   * When true, defer file-snapshot init until the Changes tab is first opened —
   * used by surfaces like terminals whose workspace may be a large arbitrary
   * directory we don't want to snapshot until the user asks. Conversations leave
   * it falsy for eager init (snapshot baseline captured at mount) so agent edits
   * made before the Changes tab is opened still surface as changes.
   */
  lazyChanges?: boolean;
  /** Called when the user selects node(s) in the tree (replace selection). */
  onSelectFiles?: (items: SelectedFile[]) => void;
  /** Called when the user appends node(s) to the active surface (e.g. "Add to chat"). */
  onAppendFiles?: (items: SelectedFile[]) => void;
  /**
   * Subscribe to external refresh triggers (agent writes, manual refresh,
   * selection-clear). Returns an unsubscribe. The callback the body passes
   * always points at the latest tree refresh; the source decides when to fire.
   */
  subscribeRefresh?: (cb: () => void) => () => void;
  /**
   * Subscribe to inbound selection-sync (e.g. the conversation SendBox closing
   * a file tag pushes the new folder selection back into the tree, #1083).
   * The source normalizes its own event payload to the folder items the tree
   * needs; the body applies them to its own selection state. Returns an
   * unsubscribe. Sources without an external selection channel (terminal) omit
   * it.
   */
  subscribeSelectionSync?: (cb: (folders: SelectedFile[]) => void) => () => void;
  /**
   * Subscribe to streamed file-tree replacements (the conversation search
   * provider pushes a single matched root node as results arrive). The body
   * replaces its tree root with the pushed node. Returns an unsubscribe.
   * Sources without a streaming search channel (terminal) omit it.
   */
  subscribeFileTreeReplace?: (cb: (root: IDirOrFile) => void) => () => void;
  /** Import capability config; presence enables paste/drag/upload (conversation only). */
  upload?: WorkspaceUploadConfig;
  /** Optional source-specific tabs rendered after Files / Changes. */
  extraTabs?: WorkspaceExtraTab[];
}

/**
 * 目标文件夹路径信息
 * Target folder path information
 */
export interface TargetFolderPath {
  fullPath: string;
  relativePath: string | null;
}

export type WorkspaceTab = 'files' | 'changes' | (string & {});
