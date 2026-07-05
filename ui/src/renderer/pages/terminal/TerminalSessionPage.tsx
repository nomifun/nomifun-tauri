/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useParams } from 'react-router-dom';
import { Button, Input, Message } from '@arco-design/web-react';
import { Refresh, EditOne, Terminal } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import AutoWorkControl from '@/renderer/pages/conversation/components/AutoWorkControl';
import IdmmControl from '@/renderer/pages/conversation/components/IdmmControl';
import KnowledgeControl from '@/renderer/pages/conversation/components/KnowledgeControl';
import { useResizableSplit } from '@/renderer/hooks/ui/useResizableSplit';
import { PreviewPanel, PreviewProvider, usePreviewContext } from '@/renderer/pages/conversation/Preview';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { isDesktopShell, isMacOS, isWindows } from '@/renderer/utils/platform';
import { useWorkspaceCollapse } from '@/renderer/pages/conversation/hooks/useWorkspaceCollapse';
import WorkspacePanelHeader, { DesktopWorkspaceToggle } from '@/renderer/pages/conversation/components/ChatLayout/WorkspacePanelHeader';
import { dispatchWorkspaceToggleEvent } from '@/renderer/utils/workspace/workspaceEvents';
import { WORKSPACE_HEADER_HEIGHT } from '@/renderer/pages/conversation/utils/layoutCalc';
import RegisterKnowledgeButton from './RegisterKnowledgeButton';
import TerminalWorkspaceRail from './TerminalWorkspaceRail';
import XtermView, { type XtermViewHandle } from './XtermView';
import TerminalSendBox from './TerminalSendBox';
import { isTerminalAutoworkCapable } from './detectFamily';
import styles from './XtermView.module.css';

/** Workspace rail width bounds (px), mirroring the conversation workspace panel. */
const TERMINAL_WORKSPACE_DEFAULT_PX = 300;
const TERMINAL_WORKSPACE_MIN_PX = 220;
const TERMINAL_WORKSPACE_MAX_PX = 560;

/** Preview column minimum width (px) so it never collapses to nothing. */
const TERMINAL_PREVIEW_MIN_PX = 260;

/**
 * TerminalRightRegion — the right side of the terminal page (preview + rail).
 *
 * Lives strictly INSIDE the terminal-scoped {@link PreviewProvider}. The preview
 * column is an independent, resizable column that only mounts when a preview is
 * open.
 *
 * The workspace rail mirrors the conversation right sider EXACTLY for toggle
 * parity (the user wants identical position/interaction):
 *  - {@link useWorkspaceCollapse} drives collapse, so the SAME global
 *    `WORKSPACE_TOGGLE_EVENT` toggles it — dispatched by the titlebar workspace
 *    button on mac/Windows (see Layout `workspaceAvailable`, now extended to
 *    `/terminal/`) or by the in-panel/floating toggle on Linux/web — and
 *    `WORKSPACE_STATE_EVENT` keeps the titlebar icon in sync.
 *  - {@link WorkspacePanelHeader} is the header, with the in-panel toggle gated
 *    to non-mac/Windows desktop (identical to ChatLayout).
 *  - The rail collapses to width 0; {@link DesktopWorkspaceToggle} is the
 *    floating expand affordance when collapsed (Linux/web only).
 *  - It explicitly auto-expands on the current terminal's
 *    `WORKSPACE_HAS_FILES_EVENT` once the cwd's files load (not a temp
 *    workspace), so a terminal opened on a populated dir shows the rail without
 *    a manual toggle.
 */
const TerminalRightRegion: React.FC<{ session: ITerminalSession }> = ({ session }) => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = Boolean(layout?.isMobile);
  const isDesktop = !isMobile;
  // Desktop-shell mac/win runtime — gate on isDesktopShell() first (matching
  // ChatLayout/Titlebar): on mac/Windows the titlebar drives the toggle, so the
  // in-panel toggle + floating expand button are hidden there; everyone else
  // (Linux desktop, WebUI browser) keeps the in-panel toggle.
  const isDesktopRuntime = isDesktopShell();
  const isMacRuntime = isDesktopRuntime && isMacOS();
  const isWindowsRuntime = isDesktopRuntime && isWindows();

  // Preview panel open state (the terminal's own provider, not the conversation's).
  const { isOpen: isPreviewOpen } = usePreviewContext();

  // Rail collapse — the SAME hook the conversation rail uses, so the titlebar
  // workspace button (WORKSPACE_TOGGLE_EVENT) toggles it and the titlebar icon
  // stays in sync (WORKSPACE_STATE_EVENT). Per-session preference key; not a
  // temp workspace, so it auto-expands once the cwd's files load.
  const { rightSiderCollapsed } = useWorkspaceCollapse({
    workspaceEnabled: true,
    isMobile,
    preferenceKey: `terminal-${session.id}`,
    isTemporaryWorkspace: false,
    autoExpandOnFiles: true,
    workspaceEventKey: String(session.id),
  });

  // Rail width (px), persisted. Drag handle on the rail's LEFT edge → reverse:true.
  const { splitRatio: railWidthPx, createDragHandle: createRailDragHandle } = useResizableSplit({
    unit: 'px',
    defaultWidth: TERMINAL_WORKSPACE_DEFAULT_PX,
    minWidth: TERMINAL_WORKSPACE_MIN_PX,
    maxWidth: TERMINAL_WORKSPACE_MAX_PX,
    storageKey: 'terminal-workspace-width-px',
  });

  // Preview column width (px), persisted independently of the rail.
  const { splitRatio: previewWidthPx, createDragHandle: createPreviewDragHandle } = useResizableSplit({
    unit: 'px',
    defaultWidth: 420,
    minWidth: TERMINAL_PREVIEW_MIN_PX,
    maxWidth: 960,
    storageKey: 'terminal-preview-width-px',
  });

  return (
    <>
      {/* Preview column — independent, only when a preview is open. The xterm
          ResizeObserver refits the terminal automatically when this mounts /
          unmounts or is dragged. */}
      {isPreviewOpen && (
        <div
          className='relative flex flex-col min-h-0 bg-1'
          style={{
            flex: `0 0 ${Math.round(previewWidthPx)}px`,
            width: `${Math.round(previewWidthPx)}px`,
            minWidth: `${TERMINAL_PREVIEW_MIN_PX}px`,
          }}
        >
          {createPreviewDragHandle({
            className: 'absolute top-0 bottom-0 left-0 z-30',
            style: { width: '12px', left: '-6px' },
            reverse: true,
            linePlacement: 'start',
          })}
          <div className='h-full w-full overflow-hidden'>
            <PreviewPanel />
          </div>
        </div>
      )}

      {/* Workspace rail — mirrors the conversation right sider: collapses to
          width 0, WorkspacePanelHeader on top (in-panel toggle gated to
          non-mac/Windows), left-edge resize handle when expanded. */}
      {!isMobile && (
        <div
          className='!bg-1 relative layout-sider'
          style={{
            flexGrow: 0,
            flexShrink: 0,
            flexBasis: rightSiderCollapsed ? '0px' : `${Math.round(railWidthPx)}px`,
            width: rightSiderCollapsed ? '0px' : `${Math.round(railWidthPx)}px`,
            minWidth: rightSiderCollapsed ? '0px' : `${TERMINAL_WORKSPACE_MIN_PX}px`,
            overflow: 'hidden',
            borderLeft: rightSiderCollapsed ? 'none' : '1px solid var(--bg-3)',
          }}
        >
          {isDesktop &&
            !rightSiderCollapsed &&
            createRailDragHandle({ className: 'absolute left-0 top-0 bottom-0', reverse: true })}
          <WorkspacePanelHeader
            showToggle={!isMacRuntime && !isWindowsRuntime}
            collapsed={rightSiderCollapsed}
            onToggle={() => dispatchWorkspaceToggleEvent()}
            togglePlacement={isMobile ? 'left' : 'right'}
            workspacePath={session.cwd}
          >
            <span className='text-14px font-medium text-t-primary truncate'>
              {t('terminal.workspace.title', { defaultValue: '项目' })}
            </span>
          </WorkspacePanelHeader>
          <div style={{ height: `calc(100% - ${WORKSPACE_HEADER_HEIGHT}px)` }}>
            <TerminalWorkspaceRail session={session} />
          </div>
        </div>
      )}

      {/* Desktop expand button when collapsed — Linux/web only (mac/Windows use
          the titlebar workspace button). */}
      {!isMacRuntime && !isWindowsRuntime && rightSiderCollapsed && !isMobile && <DesktopWorkspaceToggle />}
    </>
  );
};

const TerminalSessionPage: React.FC = () => {
  const { id } = useParams<{ id: string }>();
  const { t } = useTranslation();
  const [session, setSession] = useState<ITerminalSession | null>(null);
  const [relaunching, setRelaunching] = useState(false);
  const [fallingBack, setFallingBack] = useState(false);
  const fallingBackRef = useRef(false);
  const xtermApi = useRef<XtermViewHandle | null>(null);
  // Inline title editing in the header.
  const [editingName, setEditingName] = useState(false);
  const [draftName, setDraftName] = useState('');
  const [savingName, setSavingName] = useState(false);
  const savingNameRef = useRef(false);
  const skipBlurSaveRef = useRef(false);

  useEffect(() => {
    if (!id) return;
    // Route param is a string; the terminal session id is a numeric primary key.
    const sessionId = Number(id);
    let active = true;
    void ipcBridge.terminal.get
      .invoke({ id: sessionId })
      .then((s) => {
        if (active) setSession(s);
      })
      .catch(() => {
        /* removed */
      });

    const offExit = ipcBridge.terminal.onExit.on((evt) => {
      if (evt.id === sessionId)
        setSession((prev) => (prev ? { ...prev, last_status: 'exited', exit_code: evt.exit_code } : prev));
    });
    const offUpdated = ipcBridge.terminal.onUpdated.on((s) => {
      if (s.id === sessionId) setSession(s);
    });
    return () => {
      active = false;
      offExit();
      offUpdated();
    };
  }, [id]);

  const handleRelaunch = useCallback(async () => {
    if (!session) return;
    setRelaunching(true);
    try {
      // Relaunch in place: the backend respawns the PTY for the SAME session id
      // (a PTY child cannot be resumed once it exits, so a fresh process is
      // unavoidable, but reusing the id keeps this tab/session continuous —
      // no new sidebar entry, no session sprawl). Clear the stale output first;
      // the new process's output streams over the same WS subscription.
      const updated = await ipcBridge.terminal.relaunch.invoke({
        id: session.id,
      });
      xtermApi.current?.clear();
      xtermApi.current?.focus();
      setSession(updated);
    } catch (err) {
      Message.error(err instanceof Error ? err.message : String(err));
    } finally {
      setRelaunching(false);
    }
  }, [session]);

  // Fall back to a clean login shell in place: the escape hatch for a wedged,
  // garbled claude/codex TUI. Kills the agent and respawns the platform shell
  // under the SAME session id, then full-resets the xterm grid (exits alt-screen
  // / clears the garble). Wired to both the header button and the rapid-Ctrl+C
  // escalation. Guarded so a Ctrl+C burst + a button click cannot double-fire.
  const handleFallbackShell = useCallback(async () => {
    if (!session || fallingBackRef.current) return;
    fallingBackRef.current = true;
    setFallingBack(true);
    try {
      const updated = await ipcBridge.terminal.relaunchShell.invoke({ id: session.id });
      xtermApi.current?.reset();
      xtermApi.current?.focus();
      setSession(updated);
      Message.success(t('terminal.fallbackShellDone'));
    } catch (err) {
      Message.error(err instanceof Error ? err.message : String(err));
    } finally {
      fallingBackRef.current = false;
      setFallingBack(false);
    }
  }, [session, t]);

  const startEditName = useCallback(() => {
    if (!session) return;
    setDraftName(session.name ?? '');
    setEditingName(true);
  }, [session]);

  // Save the edited title via the same update API the sidebar rename uses; the
  // sidebar stays in sync through its own `terminal.updated` subscription.
  const saveName = useCallback(async () => {
    if (savingNameRef.current || !session) return;
    const trimmed = draftName.trim();
    // Empty or unchanged → treat as cancel; no request.
    if (!trimmed || trimmed === session.name) {
      setEditingName(false);
      return;
    }
    savingNameRef.current = true;
    setSavingName(true);
    try {
      const updated = await ipcBridge.terminal.update.invoke({
        id: session.id,
        name: trimmed,
      });
      setSession(updated);
      // Mirror cancelEditName: the unmount-triggered blur must not re-save.
      skipBlurSaveRef.current = true;
      setEditingName(false);
    } catch (err) {
      Message.error(err instanceof Error ? err.message : String(err));
    } finally {
      savingNameRef.current = false;
      setSavingName(false);
    }
  }, [session, draftName]);

  // Blur commits the edit — except right after Esc, which cancels.
  const handleNameBlur = useCallback(() => {
    if (skipBlurSaveRef.current) {
      skipBlurSaveRef.current = false;
      return;
    }
    void saveName();
  }, [saveName]);

  const cancelEditName = useCallback(() => {
    skipBlurSaveRef.current = true;
    setEditingName(false);
  }, []);

  if (!id) return null;

  // Numeric terminal session id for the numeric-id APIs and child views.
  // KnowledgeControl also takes the numeric session id; its terminal binding
  // resolves via a workpath key (not the numeric id) once the session is found.
  const sessionId = Number(id);

  const isExited = session?.last_status && session.last_status !== 'running';

  // AutoWork is only meaningful for agent-CLI terminals running in the foreground.
  // Capability is resolved from the launch command/args/backend the SAME way the
  // backend gate does (wrappers like `stepcode claude` count; a plain shell or
  // gemini does not).
  const isAgentCli = isTerminalAutoworkCapable(session?.command ?? '', session?.args ?? [], session?.backend);
  const autoWorkDisabledReason = !isAgentCli
    ? t('terminal.autowork.requiresAgentCli')
    : isExited
      ? t('terminal.autowork.terminalExited')
      : undefined;
  const autoWorkSafetyHint =
    isAgentCli && session?.mode !== 'full-auto' ? t('terminal.autowork.fullAutoHint') : undefined;

  return (
    // The WHOLE page (both columns) is wrapped in the terminal-scoped
    // PreviewProvider — not just the right region. TerminalSendBox (left column)
    // reuses the shared chat SendBox, which calls usePreviewContext()
    // (setSendBoxHandler). The global app-level PreviewProvider was removed in
    // favor of per-surface providers, so wrapping only the right region left the
    // SendBox with no provider in scope → "usePreviewContext must be used within
    // PreviewProvider" → white screen on terminal mount. subscribeGlobalOpen=
    // false keeps agent-driven global preview.open out of the terminal; the
    // `terminal` namespace isolates persisted preview tabs from conversations.
    <PreviewProvider persistNamespace='terminal' subscribeGlobalOpen={false}>
    <div className='relative flex flex-row h-full min-h-0 bg-fill-1 overflow-hidden'>
      {/* Terminal column: header + xterm + composer. flex-1 with a floor so it
          never collapses when the preview / rail columns open. */}
      <div className='flex flex-col flex-1 min-w-0 h-full' style={{ minWidth: 360 }}>
        {/* Header */}
        <div className={`${styles.header} flex items-center justify-between px-16px py-10px`}>
          <div className='flex items-center gap-8px min-w-0'>
            {editingName ? (
              <Input
                size='small'
                autoFocus
                disabled={savingName}
                value={draftName}
                onChange={setDraftName}
                onPressEnter={() => void saveName()}
                onBlur={handleNameBlur}
                onKeyDown={(e) => {
                  if (e.key === 'Escape') {
                    e.preventDefault();
                    cancelEditName();
                  }
                }}
                className='w-240px max-w-full'
              />
            ) : (
              <div
                className='group flex items-center gap-4px min-w-0 cursor-text'
                onClick={startEditName}
                title={t('terminal.action.rename')}
              >
                <span className='text-14px font-medium text-t-primary truncate'>
                  {session?.name || t('terminal.untitled')}
                </span>
                <EditOne className='shrink-0 opacity-0 group-hover:opacity-60 transition-opacity' size='14' />
              </div>
            )}
            {isExited && !editingName && (
              <span className='text-12px text-t-tertiary'>
                {t('terminal.statusExited', {
                  code: String(session?.exit_code ?? 0),
                })}
              </span>
            )}
          </div>
          <div className='flex items-center gap-8px shrink-0'>
            <KnowledgeControl
              target={{ kind: 'terminal', id: sessionId }}
              applyNote={t('terminal.knowledge.applyAfterRelaunch')}
              footer={
                <div className='flex flex-col gap-6px'>
                  <span className='text-11px leading-15px text-t-tertiary'>
                    {t('terminal.extended.knowledgeConnectNote')}
                  </span>
                  <RegisterKnowledgeButton cwd={session?.cwd ?? ''} command={session?.command ?? ''} />
                </div>
              }
            />
            <AutoWorkControl
              target={{ kind: 'terminal', id: sessionId }}
              disabledReason={autoWorkDisabledReason}
              safetyHint={autoWorkSafetyHint}
            />
            <IdmmControl target={{ kind: 'terminal', id: sessionId }} />
            {/* Escape hatch for a wedged/garbled claude/codex TUI: always
                available (NOT gated on isExited) for agent sessions. After a
                fallback the session is a plain shell, so isAgentCli flips false
                and this button disappears. */}
            {isAgentCli && (
              <Button
                size='small'
                loading={fallingBack}
                icon={<Terminal size='14' />}
                onClick={handleFallbackShell}
                title={t('terminal.fallbackShellTip')}
              >
                {t('terminal.fallbackShell')}
              </Button>
            )}
            {isExited && (
              <Button
                type='primary'
                size='small'
                loading={relaunching}
                icon={<Refresh size='14' />}
                onClick={handleRelaunch}
              >
                {t('terminal.relaunch')}
              </Button>
            )}
          </div>
        </div>

        {/* Terminal output */}
        <div className='flex-1 min-h-0 px-12px pt-12px'>
          <XtermView
            sessionId={sessionId}
            apiRef={xtermApi}
            className='h-full'
            onEscalateShell={isAgentCli ? handleFallbackShell : undefined}
          />
        </div>

        {/* Enhanced composer */}
        <div className='px-12px pt-8px pb-12px'>
          <TerminalSendBox
            sessionId={sessionId}
            terminalApi={xtermApi}
            disabled={!!isExited}
            onClearView={() => xtermApi.current?.clear()}
          />
        </div>
      </div>

      {/* Right region: preview + workspace rail. Lives inside the page-level
          PreviewProvider above. Mounted only once the session is loaded (the
          rail needs session.id / session.cwd). */}
      {session && <TerminalRightRegion session={session} />}
    </div>
    </PreviewProvider>
  );
};

export default TerminalSessionPage;
