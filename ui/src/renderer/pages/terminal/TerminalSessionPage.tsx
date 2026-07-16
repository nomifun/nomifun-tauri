/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useParams } from 'react-router-dom';
import { Button, Input, Message, Spin } from '@arco-design/web-react';
import { Refresh, EditOne, Terminal } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import type { ITerminalSession } from '@/common/adapter/ipcBridge';
import { parseTerminalId, terminalTarget, type TerminalId } from '@/common/types/ids';
import AutoWorkControl from '@/renderer/pages/conversation/components/AutoWorkControl';
import IdmmControl from '@/renderer/pages/conversation/components/IdmmControl';
import KnowledgeControl from '@/renderer/pages/conversation/components/KnowledgeControl';
import { useResizableSplit } from '@/renderer/hooks/ui/useResizableSplit';
import { PreviewPanel, PreviewProvider, usePreviewContext } from '@/renderer/pages/conversation/Preview';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { isDesktopShell, isMacOS, isWindows } from '@/renderer/utils/platform';
import { useWorkspaceCollapse } from '@/renderer/pages/conversation/hooks/useWorkspaceCollapse';
import WorkspacePanelHeader from '@/renderer/pages/conversation/components/ChatLayout/WorkspacePanelHeader';
import WorkspaceToolRail, {
  WORKSPACE_PANEL_META_EVENT,
  dispatchWorkspacePanelTabEvent,
  type WorkspacePanelMetaDetail,
} from '@/renderer/pages/conversation/components/ChatLayout/WorkspaceToolRail';
import { useWorkspacePanelTabs } from '@/renderer/pages/conversation/hooks/useWorkspacePanelTabs';
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

type TerminalLoadError = 'not-found' | 'request-failed';

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
 *  - The rail collapses to width 0 while the persistent vertical tool strip
 *    remains visible as the expand / panel-selection affordance.
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
  const workspaceTarget = terminalTarget(session.id);
  const { rightSiderCollapsed, persistRightSiderCollapsed } = useWorkspaceCollapse({
    workspaceEnabled: true,
    isMobile,
    target: workspaceTarget,
    isTemporaryWorkspace: false,
    autoExpandOnFiles: true,
  });
  const { activeWorkspaceTab, setActiveWorkspaceTab } = useWorkspacePanelTabs(workspaceTarget);
  const [workspaceChangeCount, setWorkspaceChangeCount] = useState(0);

  useEffect(() => {
    const handleMeta = (event: Event) => {
      const detail = (event as CustomEvent<WorkspacePanelMetaDetail>).detail;
      if (
        detail?.target?.kind === workspaceTarget.kind &&
        detail.target.id === workspaceTarget.id
      ) {
        setWorkspaceChangeCount(detail.changeCount);
      }
    };
    window.addEventListener(WORKSPACE_PANEL_META_EVENT, handleMeta);
    return () => window.removeEventListener(WORKSPACE_PANEL_META_EVENT, handleMeta);
  }, [workspaceTarget.id, workspaceTarget.kind]);

  const selectWorkspaceTool = (tab: string) => {
    const clickingActivePanel = !rightSiderCollapsed && activeWorkspaceTab === tab;
    setActiveWorkspaceTab(tab);
    dispatchWorkspacePanelTabEvent(tab, workspaceTarget);
    persistRightSiderCollapsed(clickingActivePanel);
  };

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

      {/* Workspace panel — mirrors the conversation right sider: collapses to
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
            onToggle={() => dispatchWorkspaceToggleEvent(workspaceTarget)}
            togglePlacement={isMobile ? 'left' : 'right'}
            workspacePath={session.cwd}
            activeTab={activeWorkspaceTab}
          >
            <span className='text-14px font-medium text-t-primary truncate'>
              {activeWorkspaceTab === 'changes'
                ? t('conversation.workspace.changes.tab')
                : t('terminal.workspace.title', { defaultValue: '项目' })}
            </span>
          </WorkspacePanelHeader>
          <div style={{ height: `calc(100% - ${WORKSPACE_HEADER_HEIGHT}px)` }}>
            <TerminalWorkspaceRail session={session} />
          </div>
        </div>
      )}

      {!isMobile && (
        <WorkspaceToolRail
          t={t}
          activeTab={activeWorkspaceTab}
          expanded={!rightSiderCollapsed}
          onSelect={selectWorkspaceTool}
          changeCount={workspaceChangeCount}
          footer={
            <button
              type='button'
              className='workspace-tool-rail__item workspace-tool-rail__item--collapse'
              onClick={() => persistRightSiderCollapsed(!rightSiderCollapsed)}
              aria-label={rightSiderCollapsed ? 'Expand workspace' : 'Collapse workspace'}
            >
              {rightSiderCollapsed ? <span>‹</span> : <span>›</span>}
            </button>
          }
        />
      )}
    </>
  );
};

const TerminalSessionContent: React.FC<{ sessionId: TerminalId }> = ({ sessionId }) => {
  const { t } = useTranslation();
  const [session, setSession] = useState<ITerminalSession | null>(null);
  const [loadError, setLoadError] = useState<TerminalLoadError | null>(null);
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [terminalError, setTerminalError] = useState<Error | null>(null);
  const [xtermAttempt, setXtermAttempt] = useState(0);
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
    let active = true;
    setSession(null);
    setLoadError(null);
    void ipcBridge.terminal.get
      .invoke({ id: sessionId })
      .then((s) => {
        if (!active) return;
        if (s) {
          setSession(s);
          return;
        }
        setLoadError('not-found');
      })
      .catch((error: unknown) => {
        if (!active) return;
        console.error('[TerminalSessionPage] Failed to load terminal session:', error);
        setLoadError('request-failed');
      });

    const offExit = ipcBridge.terminal.onExit.on((evt) => {
      if (evt.id === sessionId)
        setSession((prev) => (prev ? { ...prev, last_status: 'exited', exit_code: evt.exit_code } : prev));
    });
    const offUpdated = ipcBridge.terminal.onUpdated.on((s) => {
      if (s.id === sessionId) {
        setLoadError(null);
        if (s.last_status === 'running') setTerminalError(null);
        setSession(s);
      }
    });
    return () => {
      active = false;
      offExit();
      offUpdated();
    };
  }, [loadAttempt, sessionId]);

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
      setTerminalError(null);
      setXtermAttempt((attempt) => attempt + 1);
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
      setTerminalError(null);
      setXtermAttempt((attempt) => attempt + 1);
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

  if (!session) {
    return (
      <div className='flex h-full w-full items-center justify-center bg-fill-1 px-24px'>
        {loadError ? (
          <div role='alert' className='flex max-w-560px flex-col items-center gap-12px text-center'>
            <div className='text-14px text-t-primary'>
              {loadError === 'not-found'
                ? t('terminal.loadNotFound', { defaultValue: 'Terminal session not found.' })
                : t('terminal.loadFailed', { defaultValue: 'Failed to load terminal session.' })}
            </div>
            <Button type='primary' onClick={() => setLoadAttempt((attempt) => attempt + 1)}>
              {t('common.retry', { defaultValue: 'Retry' })}
            </Button>
          </div>
        ) : (
          <Spin />
        )}
      </div>
    );
  }

  const isExited = session.last_status !== 'running';

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
    // The terminal id is part of the namespace so separate terminal sessions
    // never restore or overwrite each other's preview tabs.
    <PreviewProvider persistNamespace={`terminal:${sessionId}`} subscribeGlobalOpen={false}>
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
                    {t('terminal.extended.knowledgeConnectNote', {
                      defaultValue:
                        '外置 CLI 可注册无密钥命令；启动时由当前系统用户专属的本地安全通道授权。',
                    })}
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
          {terminalError ? (
            <div role='alert' className={`${styles.card} flex h-full items-center justify-center px-24px`}>
              <div className='flex max-w-560px flex-col items-center gap-12px text-center'>
                <div className='text-14px text-t-primary'>
                  {t('terminal.activationFailed', { defaultValue: 'Failed to activate terminal.' })}
                </div>
                <div className='max-w-full select-text break-all text-12px text-t-tertiary'>
                  {terminalError.message}
                </div>
                <Button
                  type='primary'
                  onClick={() => {
                    setTerminalError(null);
                    setXtermAttempt((attempt) => attempt + 1);
                  }}
                >
                  {t('common.retry', { defaultValue: 'Retry' })}
                </Button>
              </div>
            </div>
          ) : (
            <XtermView
              key={`${sessionId}:${xtermAttempt}`}
              sessionId={sessionId}
              isRunning={!isExited}
              apiRef={xtermApi}
              className='h-full'
              onEscalateShell={isAgentCli ? handleFallbackShell : undefined}
              onResizeFailure={(error: unknown) =>
                setTerminalError(error instanceof Error ? error : new Error(String(error)))
              }
            />
          )}
        </div>

        {/* Enhanced composer */}
        <div className='px-12px pt-8px pb-12px'>
          <TerminalSendBox
            terminalApi={xtermApi}
            disabled={isExited || terminalError !== null}
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

const TerminalSessionPage: React.FC = () => {
  const { id } = useParams<{ id: string }>();
  if (!id) return null;

  const sessionId = parseTerminalId(id);
  // React Router reuses the same route element when only `:id` changes. Keying
  // the stateful page content prevents session A's loaded metadata, edit state,
  // or in-flight actions from being rendered against terminal B.
  return <TerminalSessionContent key={sessionId} sessionId={sessionId} />;
};

export default TerminalSessionPage;
