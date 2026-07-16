import React, { useEffect, useMemo, useRef, useState } from 'react';
import classNames from 'classnames';
import { ArrowCircleLeft, ArrowLeft, ArrowRight, ExpandLeft, ExpandRight, Plus, Terminal } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';

import { ipcBridge } from '@/common';
import {
  isSameSessionTarget,
  type SessionTarget,
} from '@/common/types/ids';
import InstantHoverTooltip from '@renderer/components/base/InstantHoverTooltip';
import MobileConversationBrand from './MobileConversationBrand';
import TitlebarLanguageMenu from './TitlebarLanguageMenu';
import WindowControls from '../WindowControls';
import { WORKSPACE_STATE_EVENT, dispatchWorkspaceToggleEvent } from '@renderer/utils/workspace/workspaceEvents';
import type { WorkspaceStateDetail } from '@renderer/utils/workspace/workspaceEvents';
import {
  SESSION_SIDER_STATE_EVENT,
  dispatchSessionSiderToggleEvent,
} from '@renderer/utils/workspace/sessionSiderEvents';
import type { SessionSiderStateDetail } from '@renderer/utils/workspace/sessionSiderEvents';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useNavigationHistory } from '@/renderer/hooks/context/NavigationHistoryContext';
import { isDesktopShell, isMacOS } from '@/renderer/utils/platform';
import { parseSessionRoute } from '@/renderer/utils/routes/sessionRoute';
import './titlebar.css';

interface TitlebarProps {
  workspaceAvailable: boolean;
}

type TitlebarIconButtonOptions = {
  tooltip: string;
  className: string;
  children: React.ReactNode;
  disabled?: boolean;
  onClick?: () => void;
};

// Claude-desktop-style sidebar toggle icon: a rounded rectangle with a vertical divider
// near the left edge, indicating a collapsible side panel. Rendered as inline SVG since
// @icon-park doesn't ship this exact shape.
//
// Uses a 48-unit viewBox to match @icon-park's stroke scale, so passing the same
// `strokeWidth` value here and to @icon-park icons produces visually identical lines.
//
// The rect spans y=10..38 (height 28), slightly taller than @icon-park's
// ArrowLeft/ArrowRight (which span y=12..36) so the sidebar icon reads a
// touch larger. The rect remains centered at y=24, matching the arrows'
// centerline so all three icons stay on the same visual baseline.
const SidebarIcon: React.FC<{ size?: number; strokeWidth?: number }> = ({ size = 18, strokeWidth = 4 }) => (
  <svg
    width={size}
    height={size}
    viewBox='0 0 48 48'
    fill='none'
    stroke='currentColor'
    strokeWidth={strokeWidth}
    strokeLinecap='round'
    strokeLinejoin='round'
    aria-hidden='true'
    focusable='false'
  >
    <rect x='6' y='10' width='36' height='28' rx='5' />
    <line x1='18' y1='10' x2='18' y2='38' />
  </svg>
);

const Titlebar: React.FC<TitlebarProps> = ({ workspaceAvailable }) => {
  const { t } = useTranslation();
  const appTitle = useMemo(() => 'NomiFun', []);
  const [workspaceCollapsed, setWorkspaceCollapsed] = useState(true);
  const [sessionSiderCollapsed, setSessionSiderCollapsed] = useState(false);
  const [mobileCenterTitle, setMobileCenterTitle] = useState(appTitle);
  const [mobileCenterOffset, setMobileCenterOffset] = useState(0);
  const layout = useLayoutContext();
  const navigationHistory = useNavigationHistory();
  const location = useLocation();
  const navigate = useNavigate();
  const containerRef = useRef<HTMLDivElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const toolbarRef = useRef<HTMLDivElement | null>(null);
  const lastNonSettingsPathRef = useRef('/guid');
  const activeWorkspaceTarget = useMemo<SessionTarget | null>(() => {
    return parseSessionRoute(location.pathname);
  }, [location.pathname]);
  const activeConversationId =
    activeWorkspaceTarget?.kind === 'conversation' ? activeWorkspaceTarget.id : null;

  // 监听工作空间折叠状态，保持按钮图标一致 / Sync workspace collapsed state for toggle button
  useEffect(() => {
    if (typeof window === 'undefined') {
      return undefined;
    }
    const handler = (event: Event) => {
      const customEvent = event as CustomEvent<WorkspaceStateDetail>;
      if (
        activeWorkspaceTarget &&
        customEvent.detail?.target &&
        isSameSessionTarget(customEvent.detail.target, activeWorkspaceTarget) &&
        typeof customEvent.detail.collapsed === 'boolean'
      ) {
        setWorkspaceCollapsed(customEvent.detail.collapsed);
      }
    };
    window.addEventListener(WORKSPACE_STATE_EVENT, handler as EventListener);
    return () => {
      window.removeEventListener(WORKSPACE_STATE_EVENT, handler as EventListener);
    };
  }, [activeWorkspaceTarget]);

  // 同步会话二级侧栏折叠状态，使标题栏开关图标保持一致
  // Sync session secondary-sidebar collapsed state for the titlebar toggle icon
  useEffect(() => {
    if (typeof window === 'undefined') {
      return undefined;
    }
    const handler = (event: Event) => {
      const customEvent = event as CustomEvent<SessionSiderStateDetail>;
      if (typeof customEvent.detail?.collapsed === 'boolean') {
        setSessionSiderCollapsed(customEvent.detail.collapsed);
      }
    };
    window.addEventListener(SESSION_SIDER_STATE_EVENT, handler as EventListener);
    return () => {
      window.removeEventListener(SESSION_SIDER_STATE_EVENT, handler as EventListener);
    };
  }, []);

  const isDesktopRuntime = isDesktopShell();
  const isMacRuntime = isDesktopRuntime && isMacOS();
  // Windows/Linux 显示自定义窗口按钮。
  const showWindowControls = isDesktopRuntime && !isMacRuntime;
  // Desktop workspace surfaces use the persistent far-right tool rail as their
  // single toggle. Mobile keeps the titlebar entry because the rail is hidden.
  const showWorkspaceButton = workspaceAvailable && Boolean(layout?.isMobile);

  const workspaceTooltip = workspaceCollapsed
    ? t('common.expandMore', { defaultValue: 'Expand workspace' })
    : t('common.collapse', { defaultValue: 'Collapse workspace' });
  const backToChatTooltip = t('common.back', { defaultValue: 'Back to Chat' });
  const isSettingsRoute = location.pathname.startsWith('/settings');
  const iconSize = 18;
  // Desktop uses slimmer strokes to match macOS-native chrome aesthetics;
  // mobile keeps the default weight so icons stay legible at larger sizes.
  const desktopIconStroke = layout?.isMobile ? undefined : 2.5;
  // 统一在标题栏左侧展示主侧栏开关 / Always expose sidebar toggle on titlebar left side
  const showSiderToggle = Boolean(layout?.setSiderCollapsed) && !(layout?.isMobile && isSettingsRoute);
  const showBackToChatButton = Boolean(layout?.isMobile && isSettingsRoute);
  const siderTooltip = layout?.siderCollapsed
    ? t('common.expandMore', { defaultValue: 'Expand sidebar' })
    : t('common.collapse', { defaultValue: 'Collapse sidebar' });
  // 前进/后退仅在桌面端显示（移动端空间有限，保留原有的返回到聊天按钮）
  // Show back/forward on desktop only; mobile keeps the existing back-to-chat button.
  const showHistoryNav = Boolean(navigationHistory) && !layout?.isMobile;
  const historyBackTooltip = t('common.historyBack', { defaultValue: 'Back' });
  const historyForwardTooltip = t('common.forward', { defaultValue: 'Forward' });
  // 会话二级侧栏开关：仅在会话区路由显示，桌面与移动端都给一个稳定的开/合入口
  // Session secondary-sidebar toggle: shown on session routes only; a stable
  // open/close entry on both desktop and mobile (mirrors the workspace toggle).
  const isSessionRoute =
    location.pathname === '/guid' ||
    location.pathname.startsWith('/conversation/') ||
    location.pathname === '/terminal-new' ||
    location.pathname.startsWith('/terminal/');
  const sessionToggleTooltip = sessionSiderCollapsed
    ? t('sessionList.expandList', { defaultValue: 'Show conversations' })
    : t('sessionList.collapseList', { defaultValue: 'Hide conversations' });

  const handleSiderToggle = () => {
    if (!showSiderToggle || !layout?.setSiderCollapsed) return;
    layout.setSiderCollapsed(!layout.siderCollapsed);
  };

  const handleWorkspaceToggle = () => {
    if (!workspaceAvailable || !activeWorkspaceTarget) {
      return;
    }
    dispatchWorkspaceToggleEvent(activeWorkspaceTarget);
  };

  const handleBackToChat = () => {
    const target = lastNonSettingsPathRef.current;
    if (target && !target.startsWith('/settings')) {
      void navigate(target);
      return;
    }
    void navigate(-1);
  };

  // Windows/Linux: double-clicking the titlebar drag region toggles maximize,
  // matching native window behavior. Tauri's `data-tauri-drag-region` does NOT
  // implement this itself; we wire it on the frontend. Skipped on macOS (the OS
  // handles double-click on the native traffic-light chrome) and in the WebUI
  // browser (no window controls — `isDesktopRuntime` gates it). Only fires when
  // the double-click lands on the drag region itself, not on a `no-drag` button
  // (those carry `data-tauri-drag-region` absence + their own handlers).
  const handleTitlebarDoubleClick = (event: React.MouseEvent<HTMLDivElement>) => {
    if (!isDesktopRuntime || isMacRuntime) return;
    const target = event.target as HTMLElement | null;
    if (!target || !target.hasAttribute('data-tauri-drag-region')) return;
    void ipcBridge.windowControls.toggleMaximize.invoke();
  };

  useEffect(() => {
    if (!isSettingsRoute) {
      const path = `${location.pathname}${location.search}${location.hash}`;
      lastNonSettingsPathRef.current = path;
      try {
        sessionStorage.setItem('nomi:last-non-settings-path', path);
      } catch {
        // ignore
      }
      return;
    }
    try {
      const stored = sessionStorage.getItem('nomi:last-non-settings-path');
      if (stored) {
        lastNonSettingsPathRef.current = stored;
      }
    } catch {
      // ignore
    }
  }, [isSettingsRoute, location.pathname, location.search, location.hash]);

  useEffect(() => {
    if (!layout?.isMobile) {
      setMobileCenterTitle(appTitle);
      return;
    }

    // Single agent mode: show conversation name
    if (!activeConversationId) {
      setMobileCenterTitle(appTitle);
      return;
    }

    let cancelled = false;
    void ipcBridge.conversation.get
      .invoke({ id: activeConversationId })
      .then((conversation) => {
        if (cancelled) return;
        setMobileCenterTitle(conversation?.name || appTitle);
      })
      .catch(() => {
        if (cancelled) return;
        setMobileCenterTitle(appTitle);
      });

    return () => {
      cancelled = true;
    };
  }, [activeConversationId, appTitle, layout?.isMobile]);

  useEffect(() => {
    if (!layout?.isMobile) {
      setMobileCenterOffset(0);
      return;
    }

    const updateOffset = () => {
      const leftWidth = menuRef.current?.offsetWidth || 0;
      const rightWidth = toolbarRef.current?.offsetWidth || 0;
      setMobileCenterOffset((leftWidth - rightWidth) / 2);
    };

    updateOffset();

    if (typeof ResizeObserver === 'undefined') {
      window.addEventListener('resize', updateOffset);
      return () => window.removeEventListener('resize', updateOffset);
    }

    const observer = new ResizeObserver(() => updateOffset());
    if (containerRef.current) observer.observe(containerRef.current);
    if (menuRef.current) observer.observe(menuRef.current);
    if (toolbarRef.current) observer.observe(toolbarRef.current);

    return () => observer.disconnect();
  }, [layout?.isMobile, showBackToChatButton, showWorkspaceButton, mobileCenterTitle]);

  const mobileCenterStyle = layout?.isMobile
    ? ({
        '--app-titlebar-mobile-center-offset': `${workspaceAvailable ? mobileCenterOffset : 0}px`,
      } as React.CSSProperties)
    : undefined;

  const menuStyle: React.CSSProperties = useMemo(() => {
    if (!isMacRuntime || !showSiderToggle) return {};
    // macOS: sit the menu buttons right next to the traffic lights (which occupy ~70px).
    // Mobile keeps its own layout (no traffic lights).
    const marginLeft = layout?.isMobile ? '0px' : '76px';
    return {
      marginLeft,
    };
  }, [isMacRuntime, showSiderToggle, layout?.isMobile]);

  const renderIconButton = ({ tooltip, className, children, disabled, onClick }: TitlebarIconButtonOptions) => (
    <InstantHoverTooltip content={tooltip} position='bottom'>
      <button type='button' className={className} onClick={onClick} disabled={disabled} aria-label={tooltip}>
        {children}
      </button>
    </InstantHoverTooltip>
  );

  return (
    <div
      ref={containerRef}
      data-tauri-drag-region
      onDoubleClick={handleTitlebarDoubleClick}
      style={mobileCenterStyle}
      className={classNames('flex items-center gap-8px app-titlebar bg-2 border-b border-[var(--border-base)]', {
        'app-titlebar--mobile': layout?.isMobile,
        'app-titlebar--mobile-conversation': layout?.isMobile && workspaceAvailable,
        'app-titlebar--desktop': isDesktopRuntime,
        'app-titlebar--mac': isMacRuntime,
      })}
    >
      <div ref={menuRef} className='app-titlebar__menu' style={menuStyle}>
        {showBackToChatButton && (
          renderIconButton({
            tooltip: backToChatTooltip,
            className: classNames('app-titlebar__button', layout?.isMobile && 'app-titlebar__button--mobile'),
            onClick: handleBackToChat,
            children: <ArrowCircleLeft theme='outline' size={iconSize} fill='currentColor' />,
          })
        )}
        {showSiderToggle && (
          renderIconButton({
            tooltip: siderTooltip,
            className: classNames('app-titlebar__button', layout?.isMobile && 'app-titlebar__button--mobile'),
            onClick: handleSiderToggle,
            children: <SidebarIcon size={iconSize} strokeWidth={desktopIconStroke} />,
          })
        )}
        {showHistoryNav && (
          <>
            {renderIconButton({
              tooltip: historyBackTooltip,
              className: 'app-titlebar__button app-titlebar__button--nav',
              onClick: () => navigationHistory?.back(),
              disabled: !navigationHistory?.canBack,
              children: <ArrowLeft theme='outline' size={iconSize} fill='currentColor' strokeWidth={desktopIconStroke} />,
            })}
            {renderIconButton({
              tooltip: historyForwardTooltip,
              className: 'app-titlebar__button app-titlebar__button--nav',
              onClick: () => navigationHistory?.forward(),
              disabled: !navigationHistory?.canForward,
              children: <ArrowRight theme='outline' size={iconSize} fill='currentColor' strokeWidth={desktopIconStroke} />,
            })}
          </>
        )}
        {!layout?.isMobile && (
          <>
            <TitlebarLanguageMenu iconSize={iconSize} strokeWidth={desktopIconStroke} />
            {renderIconButton({
              tooltip: t('terminal.newConversation'),
              className: 'app-titlebar__button app-titlebar__button--nav',
              onClick: () => navigate('/guid', { state: { resetPreset: true } }),
              children: <Plus theme='outline' size={iconSize} fill='currentColor' strokeWidth={desktopIconStroke} />,
            })}
            {renderIconButton({
              tooltip: t('terminal.newTerminal'),
              className: 'app-titlebar__button app-titlebar__button--nav',
              onClick: () => navigate('/terminal-new'),
              children: <Terminal theme='outline' size={iconSize} fill='currentColor' strokeWidth={desktopIconStroke} />,
            })}
          </>
        )}
        {isSessionRoute && (
          renderIconButton({
            tooltip: sessionToggleTooltip,
            className: 'app-titlebar__button app-titlebar__button--nav',
            onClick: () => dispatchSessionSiderToggleEvent(),
            children: sessionSiderCollapsed ? (
              <ExpandRight theme='outline' size={iconSize} fill='currentColor' strokeWidth={desktopIconStroke} />
            ) : (
              <ExpandLeft theme='outline' size={iconSize} fill='currentColor' strokeWidth={desktopIconStroke} />
            ),
          })
        )}
      </div>
      <div
        className={classNames('app-titlebar__brand', {
          'app-titlebar__brand--centered': layout?.isMobile || !location.pathname.match(/^\/conversation\//),
        })}
        aria-label={layout?.isMobile ? mobileCenterTitle : appTitle}
        title={layout?.isMobile ? mobileCenterTitle : appTitle}
      >
        {layout?.isMobile &&
          (() => {
            if (activeConversationId) {
              return <MobileConversationBrand conversation_id={activeConversationId} fallbackTitle={mobileCenterTitle} />;
            }
            return (
              <span className='app-titlebar__brand-mobile'>
                <span className='app-titlebar__brand-text'>{mobileCenterTitle}</span>
              </span>
            );
          })()}
      </div>
      <div ref={toolbarRef} className='app-titlebar__toolbar'>
        {layout?.isMobile && <div id='app-titlebar-actions-slot' className='app-titlebar__actions-slot' />}
        {showWorkspaceButton && (
          renderIconButton({
            tooltip: workspaceTooltip,
            className: classNames('app-titlebar__button', layout?.isMobile && 'app-titlebar__button--mobile'),
            onClick: handleWorkspaceToggle,
            children: workspaceCollapsed ? (
              <ExpandRight theme='outline' size={iconSize} fill='currentColor' />
            ) : (
              <ExpandLeft theme='outline' size={iconSize} fill='currentColor' />
            ),
          })
        )}
        {showWindowControls && <WindowControls />}
      </div>
    </div>
  );
};

export default Titlebar;
