/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { Suspense, useCallback, useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';
import { cleanupSiderTooltips, getSiderTooltipProps } from '@renderer/utils/ui/siderTooltip';
import { useAuth } from '@renderer/hooks/context/AuthContext';
import { useLayoutContext } from '@renderer/hooks/context/LayoutContext';
import { blurActiveElement } from '@renderer/utils/ui/focus';
import { isDesktopShell } from '@renderer/utils/platform';
import { useKnowledgeInboxPending } from '@renderer/pages/knowledge/useKnowledge';
import {
  SiderAssistantSkillsEntry,
  SiderConversationEntry,
  SiderKnowledgeEntry,
  SiderMcpEntry,
  SiderModelHubEntry,
  SiderNomiEntry,
  SiderOpenCapabilitiesEntry,
  SiderPublicServiceEntry,
  SiderRequirementsEntry,
  SiderScheduledEntry,
  SiderSectionHeader,
} from './SiderNav';
import SiderFooter from './SiderFooter';

const SettingsSider = React.lazy(() => import('@renderer/pages/settings/components/SettingsSider'));

interface SiderProps {
  onSessionClick?: () => void;
  collapsed?: boolean;
}

/**
 * Sider — the app-level primary navigation rail.
 *
 * Slimmed down to a pure capability rail: the conversation/terminal session
 * list, the create switches, and full-text search were lifted out into the
 * content-area secondary sidebar (`ConversationShell` / `ContentSider`),
 * reached via the "会话" entry. The rail holds top-level destinations grouped
 * by small-text section headers (`SiderSectionHeader`): 常用 (会话 / 桌面伙伴),
 * 对外服务 (对外伙伴), 数据空间 (知识库), 自动化 (定时任务 / 需求平台),
 * 增强工具 (助手&Skill / MCP), and a bottom-pinned 设置 group
 * (模型&Agent + the footer).
 */
const Sider: React.FC<SiderProps> = ({ onSessionClick, collapsed = false }) => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const location = useLocation();
  const { pathname, search, hash } = location;
  const { count: pendingInboxCount } = useKnowledgeInboxPending();

  const navigate = useNavigate();
  const { logout, status } = useAuth();
  const isSettings = pathname.startsWith('/settings');
  const lastNonSettingsPathRef = useRef('/guid');
  // Logout is a WebUI-only affordance: the bundled desktop shell (Electron or
  // Tauri) is single-user with no auth, so there is nothing to log out of.
  const showLogout = !isDesktopShell() && status === 'authenticated';

  useEffect(() => {
    if (!pathname.startsWith('/settings')) {
      lastNonSettingsPathRef.current = `${pathname}${search}${hash}`;
    }
  }, [pathname, search, hash]);

  const navTo = useCallback(
    (target: string) => {
      cleanupSiderTooltips();
      blurActiveElement();
      Promise.resolve(navigate(target)).catch((error) => {
        console.error('Navigation failed:', error);
      });
      if (onSessionClick) {
        onSessionClick();
      }
    },
    [navigate, onSessionClick]
  );

  const handleConversationClick = () => navTo('/guid');
  const handleScheduledClick = () => navTo('/scheduled');
  const handleRequirementsClick = () => navTo('/requirements');
  const handleKnowledgeClick = () => navTo('/knowledge');
  const handleNomiClick = () => navTo('/nomi');
  const handlePublicServiceClick = () => navTo('/public-companions');
  const handleAssistantSkillsClick = () => navTo('/assistants?tab=assistants');
  const handleMcpClick = () => navTo('/mcp');
  const handleOpenCapabilitiesClick = () => navTo('/open-capabilities');
  const handleModelHubClick = () => navTo('/models');

  const handleSettingsClick = () => {
    cleanupSiderTooltips();
    blurActiveElement();
    if (isSettings) {
      const target = lastNonSettingsPathRef.current || '/guid';
      Promise.resolve(navigate(target)).catch((error) => {
        console.error('Navigation failed:', error);
      });
    } else {
      Promise.resolve(navigate('/settings/system')).catch((error) => {
        console.error('Navigation failed:', error);
      });
    }
    if (onSessionClick) {
      onSessionClick();
    }
  };

  const handleLogout = useCallback(async () => {
    cleanupSiderTooltips();
    blurActiveElement();
    try {
      await logout();
    } catch (error) {
      console.error('Logout failed:', error);
      return; // logout 失败时不执行后续操作
    }
    if (onSessionClick) {
      onSessionClick();
    }
  }, [logout, onSessionClick]);

  useEffect(() => {
    if (!showLogout) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.shiftKey && event.key.toLowerCase() === 'l') {
        event.preventDefault();
        handleLogout();
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => {
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [handleLogout, showLogout]);

  const tooltipEnabled = collapsed && !isMobile;
  const siderTooltipProps = getSiderTooltipProps(tooltipEnabled);

  // The "会话" entry stays active across every route owned by ConversationShell.
  const isSessionRoute =
    pathname === '/guid' ||
    pathname.startsWith('/conversation/') ||
    pathname === '/terminal-new' ||
    pathname.startsWith('/terminal/');

  return (
    <div className='size-full flex flex-col'>
      {/* Main content area */}
      <div className='flex-1 min-h-0 overflow-hidden'>
        {isSettings ? (
          <Suspense fallback={<div className='size-full' />}>
            <SettingsSider collapsed={collapsed} tooltipEnabled={tooltipEnabled} />
          </Suspense>
        ) : (
          <div className='size-full flex flex-col gap-2px'>
            {/* 常用 — high-frequency primary destinations */}
            <SiderSectionHeader label={t('common.siderSection.common')} collapsed={collapsed} />
            {/* Conversations — opens the session secondary sidebar (ContentSider) */}
            <SiderConversationEntry
              isMobile={isMobile}
              isActive={isSessionRoute}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleConversationClick}
            />
            {/* Work partner (桌面伙伴) */}
            <SiderNomiEntry
              isMobile={isMobile}
              isActive={pathname.startsWith('/nomi')}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleNomiClick}
            />
            {/* 对外服务 — public-facing customer-service agents (对外伙伴), a domain
                fully separate from the desktop-companion group above. */}
            <SiderSectionHeader label={t('common.siderSection.publicService')} collapsed={collapsed} />
            <SiderPublicServiceEntry
              isMobile={isMobile}
              isActive={pathname.startsWith('/public-companions')}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handlePublicServiceClick}
            />
            {/* 数据空间 — data & storage (文件管理 reserved for later) */}
            <SiderSectionHeader label={t('common.siderSection.data')} collapsed={collapsed} />
            {/* Knowledge base */}
            <SiderKnowledgeEntry
              isMobile={isMobile}
              isActive={pathname.startsWith('/knowledge')}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleKnowledgeClick}
              dot={pendingInboxCount > 0}
            />
            {/* 自动化 — automation platforms */}
            <SiderSectionHeader label={t('common.siderSection.automation')} collapsed={collapsed} />
            {/* Scheduled tasks */}
            <SiderScheduledEntry
              isMobile={isMobile}
              isActive={pathname === '/scheduled'}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleScheduledClick}
            />
            {/* Requirements platform */}
            <SiderRequirementsEntry
              isMobile={isMobile}
              isActive={pathname.startsWith('/requirements')}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleRequirementsClick}
            />
            {/* 增强工具 — extension capabilities */}
            <SiderSectionHeader label={t('common.siderSection.tools')} collapsed={collapsed} />
            {/* Assistant & Skill — assistant presets plus reusable skill packages */}
            <SiderAssistantSkillsEntry
              isMobile={isMobile}
              isActive={pathname.startsWith('/assistants')}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleAssistantSkillsClick}
            />
            {/* MCP — MCP tool server configuration */}
            <SiderMcpEntry
              isMobile={isMobile}
              isActive={pathname.startsWith('/mcp')}
              collapsed={collapsed}
              siderTooltipProps={siderTooltipProps}
              onClick={handleMcpClick}
            />
          </div>
        )}
      </div>
      {/* Bottom pinned group (设置) — Model & Agent and Open Capabilities sit directly above Settings */}
      <div className='shrink-0 mt-auto pt-8px flex flex-col gap-2px border-t border-solid border-[var(--color-border-2)] border-l-0 border-r-0 border-b-0'>
        {/* 设置 — section label; the enclosing border-t already separates this region when collapsed */}
        <SiderSectionHeader label={t('common.siderSection.settings')} collapsed={collapsed} collapsedRule={false} />
        <SiderModelHubEntry
          isMobile={isMobile}
          isActive={pathname.startsWith('/models')}
          collapsed={collapsed}
          siderTooltipProps={siderTooltipProps}
          onClick={handleModelHubClick}
        />
        <SiderOpenCapabilitiesEntry
          isMobile={isMobile}
          isActive={pathname.startsWith('/open-capabilities')}
          collapsed={collapsed}
          siderTooltipProps={siderTooltipProps}
          onClick={handleOpenCapabilitiesClick}
        />
        <SiderFooter
          isMobile={isMobile}
          isSettings={isSettings}
          collapsed={collapsed}
          siderTooltipProps={siderTooltipProps}
          onSettingsClick={handleSettingsClick}
          showLogout={showLogout}
          onLogoutClick={handleLogout}
        />
      </div>
    </div>
  );
};

export default Sider;
