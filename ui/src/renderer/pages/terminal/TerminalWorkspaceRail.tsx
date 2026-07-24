/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import { terminalTarget } from '@/common/types/ids';
import { emitter } from '@/renderer/utils/emitter';
import WorkspaceRailBody from '@/renderer/pages/conversation/Workspace/WorkspaceRailBody';
import type { MessageApi, SelectedFile, WorkspaceSource } from '@/renderer/pages/conversation/Workspace/types';
import React, { useCallback, useMemo } from 'react';

/**
 * TerminalWorkspaceRail — 终端工作区右栏（终端源绑定）
 *
 * Thin binding that adapts a terminal session into a source-agnostic
 * {@link WorkspaceSource} and renders the shared {@link WorkspaceRailBody}. It is
 * the terminal counterpart of `ChatWorkspace`: the body itself knows nothing
 * about terminals — every terminal-specific bit is wired through the source.
 *
 * Differences from the conversation source:
 * - `lazyChanges: true` — a terminal cwd may be an arbitrary, possibly huge
 *   directory. We defer the file-snapshot init until the user opens the Changes
 *   tab, instead of snapshotting eagerly on mount.
 * - `onAppendFiles` emits `terminal.selected.file.append` (a path string list);
 *   `TerminalSendBox` consumes it to insert the path into the command box.
 * - No `onSelectFiles` (no send-box selection chips in a terminal — file-click
 *   still opens the preview via the body's `usePreviewContext`).
 * - No `subscribeRefresh` (manual refresh only — the body toolbar's refresh
 *   button suffices; a terminal has no agent-stream write signal to watch).
 * - No `upload` (no upload/drag/paste affordance in the terminal rail).
 *
 * The rail must render inside a `PreviewProvider` (the page mounts a terminal-
 * scoped one wrapping both this rail and the preview column) so the body's
 * file-click → preview routing resolves against the terminal surface.
 */
const TerminalWorkspaceRail: React.FC<{
  session: ITerminalSession;
  messageApi?: MessageApi;
}> = ({ session, messageApi }) => {
  const terminalId = session.terminal_id;
  const cwd = session.cwd;

  // Append a tree-selected path into the command box. The emitter payload type
  // accepts bare path strings; we forward only the absolute path because the
  // SendBox just needs something insertable, not the full file descriptor.
  const onAppendFiles = useCallback((items: SelectedFile[]) => {
    emitter.emit(
      'terminal.selected.file.append',
      items.map((item) => item.fullPath),
    );
  }, []);

  const source = useMemo<WorkspaceSource>(
    () => ({
      workspace: cwd,
      tree: {
        key: terminalId,
        target: terminalTarget(terminalId),
        listRoot: (search?: string) =>
          ipcBridge.terminal.getWorkspace.invoke({
            terminal_id: terminalId,
            cwd,
            path: cwd,
            search,
          }),
        listChildren: (node: { fullPath: string; relativePath: string }) =>
          ipcBridge.terminal.getWorkspace.invoke({
            terminal_id: terminalId,
            cwd,
            path: node.fullPath,
          }),
      },
      // A terminal cwd is an arbitrary directory; defer the snapshot baseline
      // until the Changes tab is first opened (see WorkspaceSource.lazyChanges).
      lazyChanges: true,
      isTemporary: false,
      onAppendFiles,
      // Intentionally omit onSelectFiles / subscribeRefresh / upload — see the
      // component doc above.
    }),
    [terminalId, cwd, onAppendFiles],
  );

  return <WorkspaceRailBody source={source} messageApi={messageApi} />;
};

export default TerminalWorkspaceRail;
