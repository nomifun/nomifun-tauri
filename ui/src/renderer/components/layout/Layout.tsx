/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { ipcBridge } from '@/common';
import { configService } from '@/common/config/configService';
import type { ICssTheme } from '@/common/config/storage';
import PwaPullToRefresh from '@/renderer/components/layout/PwaPullToRefresh';
import Titlebar from '@/renderer/components/layout/Titlebar';
import { Layout as ArcoLayout } from '@arco-design/web-react';
import classNames from 'classnames';
import React, { Suspense, useCallback, useEffect, useRef, useState } from 'react';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';
import { LayoutContext } from '@renderer/hooks/context/LayoutContext';
import { NavigationHistoryProvider } from '@renderer/hooks/context/NavigationHistoryContext';
import { WebuiServerProvider } from '@renderer/hooks/context/WebuiServerContext';
import { useDeepLink } from '@renderer/hooks/system/useDeepLink';
import { useNotificationClick } from '@renderer/hooks/system/useNotificationClick';
import { useDirectorySelection } from '@renderer/hooks/file/useDirectorySelection';
import { processCustomCss } from '@renderer/utils/theme/customCssProcessor';
import { broadcastCustomCssSync } from '@renderer/utils/theme/themeBroadcast';
import { cleanupSiderTooltips } from '@renderer/utils/ui/siderTooltip';
import { useConversationShortcuts } from '@renderer/hooks/ui/useConversationShortcuts';
import { isDesktopShell, isElectronDesktop } from '@renderer/utils/platform';
import { computeCssSyncDecision, resolveCssByActiveTheme } from '@renderer/utils/theme/themeCssSync';
import { DEFAULT_THEME_ID } from '@renderer/pages/settings/DisplaySettings/presets';
import '@renderer/styles/layout.css';

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
    style={{ display: 'inline-block', verticalAlign: 'middle' }}
  >
    <rect x='6' y='10' width='36' height='28' rx='5' />
    <line x1='18' y1='10' x2='18' y2='38' />
  </svg>
);

const useDebug = () => {
  const [count, setCount] = useState(0);
  const timer = useRef<any>(null);
  const onClick = () => {
    const open = () => {
      ipcBridge.application.openDevTools.invoke().catch((error) => {
        console.error('Failed to open dev tools:', error);
      });
      setCount(0);
    };
    if (count >= 3) {
      return open();
    }
    setCount((prev) => {
      if (prev >= 2) {
        open();
        return 0;
      }
      return prev + 1;
    });
    clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      clearTimeout(timer.current);
      setCount(0);
    }, 1000);
  };

  return { onClick };
};

const UpdateModal = React.lazy(() => import('@/renderer/components/settings/UpdateModal'));

// Primary rail width. Default slimmed from 216 → 184; the rail is now freely
// resizable by dragging its right edge (clamped to [RAIL_MIN, RAIL_MAX]) and the
// chosen width persists per device. Dragging narrower than RAIL_COLLAPSE_THRESHOLD
// snaps the rail collapsed (collapse is also toggled from the titlebar).
const DEFAULT_SIDER_WIDTH = 184;
const RAIL_MIN_WIDTH = 160;
const RAIL_MAX_WIDTH = 300;
const DESKTOP_COLLAPSED_WIDTH = 0;
const RAIL_COLLAPSE_THRESHOLD = 140;
const SIDER_DRAG_HYSTERESIS = 6;
const RAIL_WIDTH_STORAGE_KEY = 'nomifun:rail-width';
const MOBILE_SIDER_WIDTH_RATIO = 0.67;
const MOBILE_SIDER_MIN_WIDTH = 260;
const MOBILE_SIDER_MAX_WIDTH = 420;

const readStoredRailWidth = (): number => {
  if (typeof window === 'undefined') return DEFAULT_SIDER_WIDTH;
  try {
    const raw = localStorage.getItem(RAIL_WIDTH_STORAGE_KEY);
    if (raw) {
      const parsed = parseFloat(raw);
      if (!Number.isNaN(parsed) && parsed >= RAIL_MIN_WIDTH && parsed <= RAIL_MAX_WIDTH) {
        return parsed;
      }
    }
  } catch (error) {
    console.error('Failed to read rail width from localStorage:', error);
  }
  return DEFAULT_SIDER_WIDTH;
};

const detectMobileViewportOrTouch = (): boolean => {
  if (typeof window === 'undefined') return false;
  if (isDesktopShell()) {
    return window.innerWidth < 768;
  }
  const width = window.innerWidth;
  const byWidth = width < 768;
  // 仅在小屏时才将 coarse/touch 视为移动端，避免触控笔记本被误判
  // Treat touch/coarse pointer as mobile only on smaller viewports
  const smallScreen = width < 1024;
  const byMedia = window.matchMedia('(hover: none)').matches || window.matchMedia('(pointer: coarse)').matches;
  const byTouchPoints = typeof navigator !== 'undefined' && navigator.maxTouchPoints > 0;
  return byWidth || (smallScreen && (byMedia || byTouchPoints));
};

const Layout: React.FC<{
  sider: React.ReactNode;
  onSessionClick?: () => void;
}> = ({ sider, onSessionClick: _onSessionClick }) => {
  const [collapsed, setCollapsed] = useState(false);
  const [railWidth, setRailWidth] = useState<number>(() => readStoredRailWidth());
  const [isMobile, setIsMobile] = useState(false);
  const [viewportWidth, setViewportWidth] = useState<number>(() =>
    typeof window === 'undefined' ? 390 : window.innerWidth
  );
  const [customCss, setCustomCss] = useState<string>('');
  const [shouldMountUpdateModal, setShouldMountUpdateModal] = useState(false);
  const { onClick } = useDebug();
  const { contextHolder: directorySelectionContextHolder } = useDirectorySelection();
  useDeepLink();
  useNotificationClick();
  const navigate = useNavigate();
  useConversationShortcuts({ navigate });
  const location = useLocation();
  // The titlebar workspace toggle drives the right rail on the conversation and
  // terminal session pages (both render a workspace rail via the shared
  // useWorkspaceCollapse + WORKSPACE_TOGGLE_EVENT protocol).
  const workspaceAvailable =
    location.pathname.startsWith('/conversation/') ||
    location.pathname.startsWith('/terminal/');
  const collapsedRef = useRef(collapsed);
  const railWidthRef = useRef(railWidth);
  const lastCssRef = useRef('');
  const lastUiCssUpdateAtRef = useRef(0);
  const dragStateRef = useRef<{ active: boolean; startX: number; startWidth: number }>({
    active: false,
    startX: 0,
    startWidth: DEFAULT_SIDER_WIDTH,
  });
  const dragRafRef = useRef<number | null>(null);
  const dragPendingWidthRef = useRef<number | null>(null);
  const draggingSiderElRef = useRef<Element | null>(null);

  const loadAndHealCustomCss = useCallback(async () => {
    try {
      const [savedCssRaw, savedActiveThemeId, savedThemes] = await Promise.all([
        configService.get('customCss'),
        configService.get('css.activeThemeId'),
        configService.get('css.themes'),
      ]);

      // 无显式选择时回退到系统默认主题，使新用户/无偏好用户从首启即应用默认主题（而非空 CSS）。
      // Fall back to the system default theme when none is selected, so fresh users apply it from first paint.
      const activeThemeId = savedActiveThemeId || DEFAULT_THEME_ID;

      const decision = computeCssSyncDecision({
        savedCss: savedCssRaw || '',
        activeThemeId,
        savedThemes: (savedThemes || []) as ICssTheme[],
        currentUiCss: customCss,
        lastUiCssUpdateAt: lastUiCssUpdateAtRef.current,
      });

      if (decision.shouldSkipApply) {
        return;
      }

      let effectiveCss = decision.effectiveCss;

      // If the active theme resolved to empty CSS and there IS a saved activeThemeId
      // (but it no longer matches any known theme), fall back to the system default and persist.
      if (!effectiveCss && activeThemeId && activeThemeId !== DEFAULT_THEME_ID) {
        const defaultCss = resolveCssByActiveTheme(DEFAULT_THEME_ID, (savedThemes || []) as ICssTheme[]);
        effectiveCss = defaultCss;
        // Persist the fallback so Layout doesn't keep retrying
        await Promise.all([
          configService.set('css.activeThemeId', DEFAULT_THEME_ID),
          configService.set('customCss', effectiveCss),
        ]).catch((error) => {
          console.warn('Failed to persist theme fallback:', error);
        });
      } else if (decision.shouldHealStorage) {
        await configService.set('customCss', effectiveCss).catch((error) => {
          console.warn('Failed to heal custom CSS from active theme:', error);
        });
      }

      setCustomCss(effectiveCss);
      if (lastCssRef.current !== effectiveCss) {
        lastCssRef.current = effectiveCss;
        window.dispatchEvent(new CustomEvent('custom-css-updated', { detail: { customCss: effectiveCss } }));
      }
    } catch (error) {
      console.error('Failed to load or heal custom CSS:', error);
    }
  }, [customCss]);

  // 加载并监听自定义 CSS 配置 / Load & watch custom CSS configuration
  useEffect(() => {
    void loadAndHealCustomCss();

    const handleCssUpdate = (event: CustomEvent) => {
      if (event.detail?.customCss !== undefined) {
        const css = event.detail.customCss || '';
        lastCssRef.current = css;
        lastUiCssUpdateAtRef.current = Date.now();
        setCustomCss(css);
      }
    };
    const handleStorageChange = (event: StorageEvent) => {
      if (event.key && (event.key.includes('customCss') || event.key.includes('css.activeThemeId'))) {
        void loadAndHealCustomCss();
      }
    };

    window.addEventListener('custom-css-updated', handleCssUpdate as EventListener);
    window.addEventListener('storage', handleStorageChange);

    return () => {
      window.removeEventListener('custom-css-updated', handleCssUpdate as EventListener);
      window.removeEventListener('storage', handleStorageChange);
    };
  }, [loadAndHealCustomCss]);

  // Re-sync theme css on route changes, because some settings pages do not mount CssThemeSettings.
  useEffect(() => {
    void loadAndHealCustomCss();
  }, [location.pathname, location.search, location.hash, loadAndHealCustomCss]);

  // 注入自定义 CSS / Inject custom CSS into document head
  useEffect(() => {
    const styleId = 'user-defined-custom-css';

    // 跨窗口同步：把当前生效的 customCss 广播给不挂 Layout 的独立窗口（桌宠），
    // 使其气泡/输入框 chrome 实时跟随氛围预设。这是 customCss 变化的单一汇聚点
    // （apply/heal/clear 都经 setCustomCss 流到本 effect），空串也广播以便对端清除。
    broadcastCustomCssSync(customCss);

    if (!customCss) {
      document.getElementById(styleId)?.remove();
      return;
    }

    const wrappedCss = processCustomCss(customCss);

    const ensureStyleAtEnd = () => {
      let styleEl = document.getElementById(styleId) as HTMLStyleElement | null;

      if (styleEl && styleEl.textContent === wrappedCss && styleEl === document.head.lastElementChild) {
        return;
      }

      styleEl?.remove();
      styleEl = document.createElement('style');
      styleEl.id = styleId;
      styleEl.type = 'text/css';
      styleEl.textContent = wrappedCss;
      document.head.appendChild(styleEl);
    };

    ensureStyleAtEnd();

    const observer = new MutationObserver((mutations) => {
      const hasNewStyle = mutations.some((mutation) =>
        Array.from(mutation.addedNodes).some((node) => node.nodeName === 'STYLE' || node.nodeName === 'LINK')
      );

      if (hasNewStyle) {
        const element = document.getElementById(styleId);
        if (element && element !== document.head.lastElementChild) {
          ensureStyleAtEnd();
        }
      }
    });

    observer.observe(document.head, { childList: true });

    return () => {
      observer.disconnect();
      document.getElementById(styleId)?.remove();
    };
  }, [customCss]);

  // 检测移动端并响应窗口大小变化
  useEffect(() => {
    const checkMobile = () => {
      const mobile = detectMobileViewportOrTouch();
      setIsMobile(mobile);
      setViewportWidth(window.innerWidth);
    };

    // 初始检测
    checkMobile();

    // 监听窗口大小变化
    window.addEventListener('resize', checkMobile);
    return () => window.removeEventListener('resize', checkMobile);
  }, []);

  // 进入移动端后立即折叠 / Collapse immediately when switching to mobile
  useEffect(() => {
    if (!isMobile || collapsedRef.current) {
      return;
    }
    setCollapsed(true);
  }, [isMobile]);

  // 清理侧栏 Tooltip 残留节点，避免移动端路由切换后浮层卡在左上角
  useEffect(() => {
    cleanupSiderTooltips();
  }, [isMobile, collapsed, location.pathname, location.search, location.hash]);

  // Bridge Main Process logs to F12 Console
  useEffect(() => {
    const unsubscribe = ipcBridge.application.logStream.on((entry) => {
      const prefix = `%c[Main:${entry.tag}]%c ${entry.message}`;
      const style = 'color:#7c3aed;font-weight:bold';
      if (entry.level === 'error') {
        console.error(prefix, style, 'color:inherit', ...(entry.data !== undefined ? [entry.data] : []));
      } else if (entry.level === 'warn') {
        console.warn(prefix, style, 'color:inherit', ...(entry.data !== undefined ? [entry.data] : []));
      } else {
        console.log(prefix, style, 'color:inherit', ...(entry.data !== undefined ? [entry.data] : []));
      }
    });
    return () => unsubscribe();
  }, []);

  // Handle tray events from main process / 处理来自主进程的托盘事件
  useEffect(() => {
    if (!isElectronDesktop()) return;

    // Navigate to guid page when requested from tray / 托盘请求导航到 guid 页面
    const handleNavigateToGuid = () => {
      void navigate('/guid');
    };

    // Navigate to conversation when requested from tray / 托盘请求导航到对话页面
    const handleNavigateToConversation = (event: CustomEvent<{ conversation_id: string }>) => {
      void navigate(`/conversation/${event.detail.conversation_id}`);
    };

    // Open about dialog when requested from tray / 托盘请求打开关于对话框
    const handleOpenAbout = () => {
      // Navigate to settings/about page / 导航到设置/关于页面
      void navigate('/settings/about');
    };

    // Handle pause all tasks request from tray / 托盘请求暂停所有任务
    const handlePauseAllTasks = async () => {
      const { ipcBridge } = await import('@/common');
      const result = await ipcBridge.task.stopAll.invoke();
      if (result?.success) {
        // Navigate to settings page to show task status
        void navigate('/settings/system');
      }
    };

    // Handle check update request from tray / 托盘请求检查更新
    // 1. Navigate to about page / 导航到关于页面
    // 2. Trigger update modal check / 触发更新模态框检查
    const handleCheckUpdate = () => {
      void navigate('/settings/about');
      // Trigger update modal after a short delay to ensure page is loaded
      setTimeout(() => {
        window.dispatchEvent(new CustomEvent('nomifun-open-update-modal', { detail: { source: 'tray' } }));
      }, 100);
    };

    // Listen for tray events / 监听托盘事件
    window.addEventListener('tray:navigate-to-guid', handleNavigateToGuid as EventListener);
    window.addEventListener('tray:navigate-to-conversation', handleNavigateToConversation as EventListener);
    window.addEventListener('tray:open-about', handleOpenAbout as EventListener);
    window.addEventListener('tray:pause-all-tasks', handlePauseAllTasks as EventListener);
    window.addEventListener('tray:check-update', handleCheckUpdate as EventListener);

    return () => {
      window.removeEventListener('tray:navigate-to-guid', handleNavigateToGuid as EventListener);
      window.removeEventListener('tray:navigate-to-conversation', handleNavigateToConversation as EventListener);
      window.removeEventListener('tray:open-about', handleOpenAbout as EventListener);
      window.removeEventListener('tray:pause-all-tasks', handlePauseAllTasks as EventListener);
      window.removeEventListener('tray:check-update', handleCheckUpdate as EventListener);
    };
  }, [navigate]);

  const siderWidth = isMobile
    ? Math.max(
        MOBILE_SIDER_MIN_WIDTH,
        Math.min(MOBILE_SIDER_MAX_WIDTH, Math.round(viewportWidth * MOBILE_SIDER_WIDTH_RATIO))
      )
    : railWidth;
  useEffect(() => {
    collapsedRef.current = collapsed;
  }, [collapsed]);
  useEffect(() => {
    railWidthRef.current = railWidth;
  }, [railWidth]);

  const beginSiderResizeDrag = useCallback(
    (event: React.MouseEvent<HTMLDivElement>) => {
      if (isMobile) return;
      event.preventDefault();
      dragStateRef.current = {
        active: true,
        startX: event.clientX,
        startWidth: collapsedRef.current ? DESKTOP_COLLAPSED_WIDTH : railWidthRef.current,
      };
      document.body.style.cursor = 'col-resize';
      document.body.style.userSelect = 'none';
      // Disable the sider width transition during drag for 1:1 cursor tracking.
      const siderEl = (event.currentTarget as HTMLElement).closest('.layout-sider');
      draggingSiderElRef.current = siderEl;
      siderEl?.classList.add('layout-sider--dragging');
    },
    [isMobile]
  );

  // Double-click the drag handle to restore the default rail width.
  const resetSiderWidth = useCallback(() => {
    if (isMobile) return;
    setCollapsed(false);
    railWidthRef.current = DEFAULT_SIDER_WIDTH;
    setRailWidth(DEFAULT_SIDER_WIDTH);
    try {
      localStorage.setItem(RAIL_WIDTH_STORAGE_KEY, String(DEFAULT_SIDER_WIDTH));
    } catch (error) {
      console.error('Failed to persist rail width:', error);
    }
  }, [isMobile]);

  useEffect(() => {
    const applyPendingWidth = () => {
      if (dragPendingWidthRef.current === null) return;
      const next = dragPendingWidthRef.current;
      railWidthRef.current = next;
      setRailWidth(next);
    };

    const handleMouseMove = (event: MouseEvent) => {
      const dragState = dragStateRef.current;
      if (!dragState.active) return;

      const proposedWidth = dragState.startWidth + (event.clientX - dragState.startX);
      const wasCollapsed = collapsedRef.current;
      // Hysteresis zone avoids rapid toggling right around the collapse threshold.
      const shouldCollapse = wasCollapsed
        ? proposedWidth < RAIL_COLLAPSE_THRESHOLD + SIDER_DRAG_HYSTERESIS
        : proposedWidth <= RAIL_COLLAPSE_THRESHOLD - SIDER_DRAG_HYSTERESIS;
      if (shouldCollapse !== wasCollapsed) {
        setCollapsed(shouldCollapse);
      }
      if (shouldCollapse) return;

      const clamped = Math.max(RAIL_MIN_WIDTH, Math.min(RAIL_MAX_WIDTH, proposedWidth));
      dragPendingWidthRef.current = clamped;
      if (dragRafRef.current === null) {
        dragRafRef.current = requestAnimationFrame(() => {
          dragRafRef.current = null;
          applyPendingWidth();
        });
      }
    };

    const endDrag = () => {
      if (!dragStateRef.current.active) return;
      dragStateRef.current.active = false;
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      if (dragRafRef.current !== null) {
        cancelAnimationFrame(dragRafRef.current);
        dragRafRef.current = null;
      }
      applyPendingWidth();
      dragPendingWidthRef.current = null;
      draggingSiderElRef.current?.classList.remove('layout-sider--dragging');
      draggingSiderElRef.current = null;
      try {
        localStorage.setItem(RAIL_WIDTH_STORAGE_KEY, String(railWidthRef.current));
      } catch (error) {
        console.error('Failed to persist rail width:', error);
      }
    };

    const handleBlur = () => endDrag();
    window.addEventListener('mousemove', handleMouseMove);
    window.addEventListener('mouseup', endDrag);
    window.addEventListener('blur', handleBlur);
    return () => {
      window.removeEventListener('mousemove', handleMouseMove);
      window.removeEventListener('mouseup', endDrag);
      window.removeEventListener('blur', handleBlur);
      endDrag();
    };
  }, []);

  const siderStyle = isMobile
    ? {
        position: 'fixed' as const,
        left: 0,
        zIndex: 100,
        transform: collapsed ? 'translateX(-100%)' : 'translateX(0)',
        transition: 'none',
        pointerEvents: collapsed ? ('none' as const) : ('auto' as const),
      }
    : {
        position: 'relative' as const,
        overflow: 'visible' as const,
      };

  return (
    <LayoutContext.Provider value={{ isMobile, siderCollapsed: collapsed, setSiderCollapsed: setCollapsed }}>
      <NavigationHistoryProvider>
        <WebuiServerProvider>
          <div className='app-shell flex flex-col size-full min-h-0'>
            <Titlebar workspaceAvailable={workspaceAvailable} />
          {/* 移动端左侧边栏蒙板 / Mobile left sider backdrop */}
          {isMobile && !collapsed && (
            <div className='fixed inset-0 bg-black/30 z-90' onClick={() => setCollapsed(true)} aria-hidden='true' />
          )}

          <ArcoLayout className={'size-full layout flex-1 min-h-0'}>
            <ArcoLayout.Sider
              collapsedWidth={isMobile ? 0 : 0}
              collapsed={collapsed}
              width={siderWidth}
              className={classNames('!bg-2 layout-sider', {
                collapsed: collapsed,
              })}
              style={siderStyle}
            >
              <ArcoLayout.Header
                className={classNames(
                  'flex items-center justify-start pt-8px pb-8px pl-18px pr-16px gap-12px layout-sider-header',
                  isMobile && 'layout-sider-header--mobile',
                  {
                    'cursor-pointer group ': collapsed,
                  }
                )}
              >
                <div
                  className={classNames('shrink-0 size-32px relative rd-0.5rem overflow-hidden', {
                    '!size-24px': collapsed,
                  })}
                  onClick={onClick}
                >
                  <svg className='absolute inset-0 w-full h-full' viewBox='0 0 80 80' fill='none'>
                    <defs>
                      <linearGradient id='sidebar-logo-bg' x1='0' y1='0' x2='0' y2='80' gradientUnits='userSpaceOnUse'>
                        <stop offset='0' stopColor='#1B1822'></stop>
                        <stop offset='1' stopColor='#0B0A10'></stop>
                      </linearGradient>
                      <linearGradient id='sidebar-logo-bowl' x1='15' y1='49' x2='65' y2='69' gradientUnits='userSpaceOnUse'>
                        <stop offset='0' stopColor='#FF9FB4'></stop>
                        <stop offset='1' stopColor='#FF6F91'></stop>
                      </linearGradient>
                    </defs>
                    <rect width='80' height='80' fill='url(#sidebar-logo-bg)'></rect>
                    <path key='logo-steam-1' d='M33 17 q-4.5 -4 0 -8.5' stroke='#FF8FA8' strokeWidth='3' fill='none' strokeLinecap='round'></path>
                    <path key='logo-steam-2' d='M40 15 q-4.5 -4 0 -8.5' stroke='#FFB3C4' strokeWidth='3' fill='none' strokeLinecap='round'></path>
                    <path key='logo-steam-3' d='M47 17 q-4.5 -4 0 -8.5' stroke='#FF8FA8' strokeWidth='3' fill='none' strokeLinecap='round'></path>
                    <path key='logo-rice' d='M22 46 Q22 27 40 27 Q58 27 58 46 Z' fill='#FFFFFF'></path>
                    <path key='logo-bowl' d='M14 49 H66 Q61.5 70 40 70 Q18.5 70 14 49 Z' fill='url(#sidebar-logo-bowl)'></path>
                  </svg>
                </div>
                <div className='text-16px text-t-primary collapsed-hidden font-semibold'>NomiFun</div>
                {isMobile && !collapsed && (
                  <button
                    type='button'
                    className='app-titlebar__button app-titlebar__button--mobile'
                    onClick={() => setCollapsed(true)}
                    title='Collapse sidebar'
                    aria-label='Collapse sidebar'
                  >
                    <SidebarIcon size={18} strokeWidth={2.5} />
                  </button>
                )}
                {/* 侧栏折叠改由标题栏统一控制 / Sidebar folding handled by Titlebar toggle */}
              </ArcoLayout.Header>
              <ArcoLayout.Content className='pt-0 px-8px pb-0 layout-sider-content'>
                {React.isValidElement(sider)
                  ? React.cloneElement(sider, {
                      onSessionClick: () => {
                        cleanupSiderTooltips();
                        if (isMobile) setCollapsed(true);
                      },
                      collapsed,
                    } as any)
                  : sider}
              </ArcoLayout.Content>
              {!isMobile && (
                <div
                  className='absolute top-0 h-full w-8px z-20 cursor-col-resize group'
                  style={{ right: '-4px' }}
                  onMouseDown={beginSiderResizeDrag}
                  onDoubleClick={resetSiderWidth}
                  aria-hidden='true'
                >
                  <div className='absolute top-0 left-1/2 h-full w-1px -translate-x-1/2 bg-transparent group-hover:bg-[var(--color-border-2)] transition-colors duration-150' />
                </div>
              )}
            </ArcoLayout.Sider>

            <ArcoLayout.Content
              className={'bg-1 layout-content flex flex-col min-h-0'}
              onClick={() => {
                if (isMobile && !collapsed) setCollapsed(true);
              }}
              style={
                isMobile
                  ? {
                      width: '100%',
                    }
                  : undefined
              }
            >
              <Outlet />
              {directorySelectionContextHolder}
              <PwaPullToRefresh />
              <Suspense fallback={null}>
                <UpdateModal />
              </Suspense>
            </ArcoLayout.Content>
          </ArcoLayout>
        </div>
        </WebuiServerProvider>
      </NavigationHistoryProvider>
    </LayoutContext.Provider>
  );
};

export default Layout;
