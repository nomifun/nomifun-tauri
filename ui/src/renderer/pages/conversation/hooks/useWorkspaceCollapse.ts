import { blurActiveElement } from '@/renderer/utils/ui/focus';
import {
  WORKSPACE_HAS_FILES_EVENT,
  WORKSPACE_TOGGLE_EVENT,
  dispatchWorkspaceStateEvent,
  type WorkspaceHasFilesDetail,
} from '@/renderer/utils/workspace/workspaceEvents';
import { useEffect, useRef, useState } from 'react';

type UseWorkspaceCollapseParams = {
  workspaceEnabled: boolean;
  isMobile: boolean;
  /**
   * Identifier whose change forces a mobile collapse (typically the active
   * conversation id; in team mode, the active agent's conversation id).
   */
  conversation_id?: number;
  /**
   * Stable key used to persist the user's manual toggle preference. Single-chat
   * uses `conversation_id`; team mode passes `team_id` so the preference
   * survives agent-tab switches and follows the team as a whole.
   */
  preferenceKey?: string;
  /**
   * True when the current workspace is an auto-created temporary one (no folder
   * picked by the user). When file-driven auto-expand is enabled, initial temp
   * workspace files are ignored so "send 你好 without picking a folder" leaves
   * the panel collapsed.
   */
  isTemporaryWorkspace?: boolean;
  /**
   * When false, file-tree presence signals do not open the right rail. Manual
   * toggles and persisted manual preferences still work.
   */
  autoExpandOnFiles?: boolean;
  /**
   * String identity emitted by the workspace tree. Filters the global
   * WORKSPACE_HAS_FILES_EVENT channel so another mounted rail cannot affect this
   * one.
   */
  workspaceEventKey?: string;
};

type UseWorkspaceCollapseReturn = {
  rightSiderCollapsed: boolean;
  setRightSiderCollapsed: React.Dispatch<React.SetStateAction<boolean>>;
  persistRightSiderCollapsed: (collapsed: boolean) => void;
};

type WorkspaceCollapsePreference = 'expanded' | 'collapsed' | null;

type ResolveWorkspaceCollapseAfterHasFilesParams = {
  currentCollapsed: boolean;
  detail: WorkspaceHasFilesDetail;
  isMobile: boolean;
  autoExpandOnFiles: boolean;
  isTemporaryWorkspace?: boolean;
  userPreference: WorkspaceCollapsePreference;
  workspaceEventKey?: string;
};

export function resolveWorkspaceCollapseAfterHasFiles({
  currentCollapsed,
  detail,
  isMobile,
  autoExpandOnFiles,
  isTemporaryWorkspace,
  userPreference,
  workspaceEventKey,
}: ResolveWorkspaceCollapseAfterHasFilesParams): boolean {
  if (workspaceEventKey && detail.conversation_id !== workspaceEventKey) {
    return currentCollapsed;
  }

  if (isMobile) {
    return true;
  }

  if (userPreference) {
    return userPreference === 'collapsed';
  }

  if (!autoExpandOnFiles) {
    return currentCollapsed;
  }

  const isUserPicked = !isTemporaryWorkspace;
  const isMidSession = !detail.isInitial;
  const allowAutoExpand = isUserPicked || isMidSession;
  if (allowAutoExpand && detail.hasFiles) {
    return false;
  }
  if (!detail.hasFiles) {
    return true;
  }
  return currentCollapsed;
}

/**
 * Manages workspace panel collapse/expand state.
 *
 * Default: collapsed. If enabled, file-driven auto-expand fires when
 * WORKSPACE_HAS_FILES_EVENT arrives and either:
 *   - the workspace is user-picked (folder chosen at creation), or
 *   - files appear mid-session in a temporary workspace (e.g. agent writes a
 *     file while the user is here).
 *
 * Manual toggle is persisted under `workspace-preference-${preferenceKey}` and
 * overrides auto-expand. The caller decides what `preferenceKey` is — single
 * chats use `conversation_id`, teams use `team_id`.
 *
 * Known limitation: leaving and re-entering a temporary workspace remounts the
 * workspace tree, so files added while away report as initial load. They will
 * not trigger auto-expand on return — the user must open the panel manually
 * that one time.
 */
export function useWorkspaceCollapse({
  workspaceEnabled,
  isMobile,
  conversation_id,
  preferenceKey,
  isTemporaryWorkspace,
  autoExpandOnFiles = true,
  workspaceEventKey = conversation_id != null ? String(conversation_id) : undefined,
}: UseWorkspaceCollapseParams): UseWorkspaceCollapseReturn {
  // Workspace panel always starts collapsed; manual toggles and allowed file
  // signals can expand it. See WORKSPACE_HAS_FILES_EVENT handler below.
  const [rightSiderCollapsed, setRightSiderCollapsed] = useState(true);

  // Mirror ref for collapse state
  const rightCollapsedRef = useRef(rightSiderCollapsed);

  const persistRightSiderCollapsed = (collapsed: boolean) => {
    setRightSiderCollapsed(collapsed);
    if (!preferenceKey) return;
    try {
      localStorage.setItem(`workspace-preference-${preferenceKey}`, collapsed ? 'collapsed' : 'expanded');
    } catch {
      // ignore errors
    }
  };

  // Keep ref in sync
  useEffect(() => {
    rightCollapsedRef.current = rightSiderCollapsed;
  }, [rightSiderCollapsed]);

  // Listen for workspace toggle events
  useEffect(() => {
    if (typeof window === 'undefined') {
      return undefined;
    }
    const handleWorkspaceToggle = () => {
      if (!workspaceEnabled) {
        return;
      }
      setRightSiderCollapsed((prev) => {
        const newState = !prev;
        if (preferenceKey) {
          try {
            localStorage.setItem(`workspace-preference-${preferenceKey}`, newState ? 'collapsed' : 'expanded');
          } catch {
            // ignore errors
          }
        }
        return newState;
      });
    };
    window.addEventListener(WORKSPACE_TOGGLE_EVENT, handleWorkspaceToggle);
    return () => {
      window.removeEventListener(WORKSPACE_TOGGLE_EVENT, handleWorkspaceToggle);
    };
  }, [workspaceEnabled, preferenceKey]);

  // Auto expand/collapse workspace panel based on files state (user preference takes priority)
  useEffect(() => {
    if (typeof window === 'undefined' || !workspaceEnabled) {
      return undefined;
    }
    const handleHasFiles = (event: Event) => {
      const detail = (event as CustomEvent<WorkspaceHasFilesDetail>).detail;

      // Check if user has manual preference
      let userPreference: WorkspaceCollapsePreference = null;
      if (preferenceKey) {
        try {
          const stored = localStorage.getItem(`workspace-preference-${preferenceKey}`);
          if (stored === 'expanded' || stored === 'collapsed') {
            userPreference = stored;
          }
        } catch {
          // ignore errors
        }
      }

      const nextCollapsed = resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: rightSiderCollapsed,
        detail,
        isMobile,
        autoExpandOnFiles,
        isTemporaryWorkspace,
        userPreference,
        workspaceEventKey,
      });
      if (nextCollapsed !== rightSiderCollapsed) {
        setRightSiderCollapsed(nextCollapsed);
      }
    };
    window.addEventListener(WORKSPACE_HAS_FILES_EVENT, handleHasFiles);
    return () => {
      window.removeEventListener(WORKSPACE_HAS_FILES_EVENT, handleHasFiles);
    };
  }, [
    isMobile,
    workspaceEnabled,
    rightSiderCollapsed,
    isTemporaryWorkspace,
    preferenceKey,
    autoExpandOnFiles,
    workspaceEventKey,
  ]);

  // Broadcast workspace state event
  useEffect(() => {
    if (!workspaceEnabled) {
      dispatchWorkspaceStateEvent(true);
      return;
    }
    dispatchWorkspaceStateEvent(rightSiderCollapsed);
  }, [rightSiderCollapsed, workspaceEnabled]);

  // Force collapse when workspace is disabled
  useEffect(() => {
    if (!workspaceEnabled) {
      setRightSiderCollapsed(true);
    }
  }, [workspaceEnabled]);

  // Mobile: force collapse when entering mobile mode
  useEffect(() => {
    if (!workspaceEnabled || !isMobile || rightCollapsedRef.current) {
      return;
    }
    setRightSiderCollapsed(true);
  }, [isMobile, workspaceEnabled]);

  // Mobile: force collapse workspace on conversation switch to prevent overlay
  useEffect(() => {
    if (!workspaceEnabled || !isMobile) {
      return;
    }
    setRightSiderCollapsed(true);
  }, [conversation_id, isMobile, workspaceEnabled]);

  // Mobile: blur active element on conversation switch to prevent soft keyboard
  useEffect(() => {
    if (!isMobile) {
      return;
    }
    const rafId = requestAnimationFrame(() => {
      blurActiveElement();
    });
    return () => cancelAnimationFrame(rafId);
  }, [conversation_id, isMobile]);

  return { rightSiderCollapsed, setRightSiderCollapsed, persistRightSiderCollapsed };
}
