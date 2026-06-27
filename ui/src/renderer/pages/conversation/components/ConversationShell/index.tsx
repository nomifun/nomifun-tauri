/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Outlet, useNavigate } from 'react-router-dom';
import { ipcBridge } from '@/common';
import { Message } from '@arco-design/web-react';
import ContentSider, { useContentSiderCollapse } from '@renderer/components/layout/ContentSider';
import { addRecentWorkspace } from '@renderer/components/workspace';
import { useResizableSplit } from '@renderer/hooks/ui/useResizableSplit';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import WorkpathSessionList from '@renderer/pages/conversation/SessionList';
import { useSidebarDisplayPreferences } from '@renderer/pages/conversation/SessionList/hooks/useSidebarDisplayPreferences';
import { addProjectWorkpath } from '@renderer/pages/conversation/SessionList/utils/projectWorkpaths';
import { SESSION_SIDER_TOGGLE_EVENT, dispatchSessionSiderStateEvent } from '@renderer/utils/workspace/sessionSiderEvents';
import SessionCreateBar from './SessionCreateBar';

const SESSION_SIDER_STORAGE_KEY = 'nomifun:session-sider-collapsed';
const SESSION_SIDER_WIDTH_STORAGE_KEY = 'nomifun:session-sider-width';
const SESSION_SIDER_DEFAULT_WIDTH = 300;
const SESSION_SIDER_MIN_WIDTH = 240;
const SESSION_SIDER_MAX_WIDTH = 480;

/**
 * ConversationShell — the layout route wrapping the session section
 * (`/guid`, `/conversation/:id`, `/terminal-new`, `/terminal/:id`).
 *
 * Renders the session secondary sidebar (a {@link ContentSider} hosting the
 * unified workpath session list + {@link SessionCreateBar}) on the left and the
 * matched child route in `<Outlet/>` on the right.
 *
 * Collapse behavior:
 *  - Desktop: inline panel; collapse state persists via `useContentSiderCollapse`.
 *  - Mobile: overlay drawer with a backdrop, kept closed by default.
 *
 * The collapse toggle is owned by the titlebar (a stable, always-present
 * control mirroring the workspace panel) and reaches us over the
 * session-sider event bus — so the toggle never moves around regardless of
 * collapse state. We broadcast our state back so the titlebar icon stays in sync.
 */
const ConversationShell: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const navigate = useNavigate();

  // Desktop collapse: persisted. Mobile open: transient (overlay), never
  // persisted so it can't leak across form factors.
  const desktop = useContentSiderCollapse(SESSION_SIDER_STORAGE_KEY, false);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [batchMode, setBatchMode] = useState(false);
  const {
    preferences: displayPreferences,
    applyPreset: applyDisplayPreset,
    updatePreference: updateDisplayPreference,
  } = useSidebarDisplayPreferences();

  // Drag-to-resize the expanded panel width (desktop only); persisted per device.
  const sessionResize = useResizableSplit({
    unit: 'px',
    defaultWidth: SESSION_SIDER_DEFAULT_WIDTH,
    minWidth: SESSION_SIDER_MIN_WIDTH,
    maxWidth: SESSION_SIDER_MAX_WIDTH,
    storageKey: SESSION_SIDER_WIDTH_STORAGE_KEY,
  });

  const collapsed = isMobile ? !mobileOpen : desktop.collapsed;

  const collapse = useCallback(() => {
    if (isMobile) setMobileOpen(false);
    else desktop.setCollapsed(true);
  }, [isMobile, desktop]);

  const toggle = useCallback(() => {
    if (isMobile) setMobileOpen((prev) => !prev);
    else desktop.toggle();
  }, [isMobile, desktop]);

  // Leaving mobile (e.g. window resized to desktop) clears the transient overlay.
  useEffect(() => {
    if (!isMobile) setMobileOpen(false);
  }, [isMobile]);

  // Broadcast collapse state so the titlebar toggle reflects it (mount + change).
  useEffect(() => {
    dispatchSessionSiderStateEvent(collapsed);
  }, [collapsed]);

  // The titlebar toggle drives collapse via the event bus.
  useEffect(() => {
    if (typeof window === 'undefined') return undefined;
    const handler = () => toggle();
    window.addEventListener(SESSION_SIDER_TOGGLE_EVENT, handler);
    return () => window.removeEventListener(SESSION_SIDER_TOGGLE_EVENT, handler);
  }, [toggle]);

  const handleSessionClick = useCallback(() => {
    if (isMobile) setMobileOpen(false);
  }, [isMobile]);

  const handleNewChat = useCallback(() => {
    setBatchMode(false);
    if (isMobile) setMobileOpen(false);
    void navigate('/guid', { state: { resetAssistant: true } });
  }, [isMobile, navigate]);

  const handleNewTerminal = useCallback(() => {
    setBatchMode(false);
    if (isMobile) setMobileOpen(false);
    void navigate('/terminal-new');
  }, [isMobile, navigate]);

  const handleCreateProject = useCallback(async () => {
    setBatchMode(false);
    try {
      const paths = await ipcBridge.dialog.showOpen.invoke({ properties: ['openDirectory', 'createDirectory'] });
      const projectPath = paths?.[0]?.trim();
      if (!projectPath) return;
      addProjectWorkpath(projectPath);
      addRecentWorkspace(projectPath);
      void navigate('/guid', { state: { workspace: projectPath } });
      if (isMobile) setMobileOpen(false);
      Message.success(t('sessionList.createProjectSuccess'));
    } catch (error) {
      console.error('[ConversationShell] Failed to create project:', error);
      Message.error(t('sessionList.createProjectFailed'));
    }
  }, [isMobile, navigate, t]);

  const handleConversationSelect = useCallback(() => {
    setBatchMode(false);
  }, []);

  const header = (
    <SessionCreateBar
      batchMode={batchMode}
      onToggleBatchMode={() => setBatchMode((prev) => !prev)}
      onNewChat={handleNewChat}
      onNewTerminal={handleNewTerminal}
      onCreateProject={handleCreateProject}
      displayPreferences={displayPreferences}
      onDisplayPresetChange={applyDisplayPreset}
      onDisplayPreferenceChange={updateDisplayPreference}
      onCollapse={collapse}
      onSessionClick={isMobile ? handleSessionClick : undefined}
      onConversationSelect={handleConversationSelect}
    />
  );

  const panel = (
    <ContentSider
      width={isMobile ? SESSION_SIDER_DEFAULT_WIDTH : sessionResize.splitRatio}
      header={header}
      ariaLabel={t('sessionList.title')}
      resizeHandle={isMobile ? undefined : sessionResize.createDragHandle({ className: 'right-0' })}
    >
      <div className='px-8px pb-8px'>
        <WorkpathSessionList
          collapsed={false}
          tooltipEnabled={false}
          batchMode={batchMode}
          displayPreferences={displayPreferences}
          onBatchModeChange={setBatchMode}
          onSessionClick={isMobile ? handleSessionClick : undefined}
        />
      </div>
    </ContentSider>
  );

  return (
    <div className='relative flex size-full min-h-0'>
      {!collapsed &&
        (isMobile ? (
          <>
            <div className='absolute inset-0 z-20 bg-[rgba(0,0,0,0.45)]' onClick={collapse} />
            <div className='absolute inset-y-0 left-0 z-30 h-full'>{panel}</div>
          </>
        ) : (
          panel
        ))}
      <div className='flex-1 min-w-0 min-h-0 flex flex-col'>
        <Outlet />
      </div>
    </div>
  );
};

export default ConversationShell;
