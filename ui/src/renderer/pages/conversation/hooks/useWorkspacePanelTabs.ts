/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { WorkspaceTab } from '@/renderer/pages/conversation/Workspace/types';
import {
  WORKSPACE_PANEL_TAB_EVENT,
  type WorkspacePanelTabDetail,
} from '@/renderer/pages/conversation/components/ChatLayout/WorkspaceToolRail';
import { useEffect, useState } from 'react';

const workspacePanelTabStorageKey = (preferenceKey?: string) =>
  preferenceKey ? `workspace-panel-tab-${preferenceKey}` : null;

function readStoredTab(preferenceKey?: string): WorkspaceTab {
  const key = workspacePanelTabStorageKey(preferenceKey);
  if (!key || typeof window === 'undefined') return 'files';
  try {
    return localStorage.getItem(key) || 'files';
  } catch {
    return 'files';
  }
}

export function useWorkspacePanelTabs(preferenceKey?: string): {
  activeWorkspaceTab: WorkspaceTab;
  setActiveWorkspaceTab: (tab: WorkspaceTab) => void;
} {
  const [activeWorkspaceTab, setActiveWorkspaceTabState] = useState<WorkspaceTab>(() => readStoredTab(preferenceKey));
  const eventSourceKey = preferenceKey?.replace(/^terminal-/, '');

  useEffect(() => {
    setActiveWorkspaceTabState(readStoredTab(preferenceKey));
  }, [preferenceKey]);

  useEffect(() => {
    if (typeof window === 'undefined') return undefined;
    const onTabEvent = (event: Event) => {
      const detail = (event as CustomEvent<WorkspacePanelTabDetail>).detail;
      if (detail?.sourceKey && detail.sourceKey !== eventSourceKey) {
        return;
      }
      const tab = detail?.tab;
      if (tab) setActiveWorkspaceTabState(tab);
    };
    window.addEventListener(WORKSPACE_PANEL_TAB_EVENT, onTabEvent);
    return () => window.removeEventListener(WORKSPACE_PANEL_TAB_EVENT, onTabEvent);
  }, [eventSourceKey]);

  const setActiveWorkspaceTab = (tab: WorkspaceTab) => {
    setActiveWorkspaceTabState(tab);
    const key = workspacePanelTabStorageKey(preferenceKey);
    if (!key) return;
    try {
      localStorage.setItem(key, tab);
    } catch {
      /* ignore storage failures */
    }
  };

  return { activeWorkspaceTab, setActiveWorkspaceTab };
}
