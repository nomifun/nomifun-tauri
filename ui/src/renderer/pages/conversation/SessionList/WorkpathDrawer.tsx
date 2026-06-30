/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Checkbox, Dropdown, Menu, Tooltip } from '@arco-design/web-react';
import { BookOne, BranchOne, DeleteOne, FolderClose, FolderOpen, Home, MessageOne, Plus, Pushpin, Terminal } from '@icon-park/react';
import classNames from 'classnames';
import React, { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import CapabilityIcon, { CAPABILITY_COLORS } from '@/renderer/components/capability/CapabilityIcon';
import CopyIconButton from '@/renderer/components/base/CopyIconButton';
import PathText from '@/renderer/components/base/PathText';

import SessionKindGroup from './SessionKindGroup';
import type { WorkpathUiState } from './hooks/useWorkpathUiState';
import { useWorkpathKnowledgeLit } from './hooks/useWorkpathKnowledge';
import {
  getBatchSelectionScopeState,
  getWorkpathBatchSelectionScope,
  type BatchSelectableScope,
  type BatchSelectionState,
} from './utils/batchSelectionScopes';
import { DEFAULT_WORKPATH_KEY } from './utils/workpathKey';
import {
  getVisibleWorkpathEntries,
  getWorkpathEntryDisplayIndex,
  WORKPATH_COLLAPSED_SESSION_LIMIT,
} from './utils/workpathVisibleEntries';
import { getRenderedExpansionState } from './utils/workpathExpansion';
import {
  DEFAULT_SIDEBAR_DISPLAY_PREFERENCES,
  formatWorkpathDisplay,
  type SidebarDisplayPreferences,
} from './utils/sidebarDisplayPreferences';
import type { SessionEntry, SessionKind, WorkpathNode } from './utils/workpathTree';

export interface WorkpathDrawerProps {
  node: WorkpathNode;
  ui: WorkpathUiState;
  /**
   * The session id currently open via `/conversation/:id` or `/terminal/:id`,
   * coerced to the numeric session id (route strings are coerced at the
   * SessionList boundary). When it belongs to this node, the drawer (and the
   * containing subgroup) is forced open — visually only, never written back to
   * localStorage.
   */
  activeSessionId: number | null;
  onCreateInteractive: (node: WorkpathNode) => void;
  onCreateTerminal: (node: WorkpathNode) => void;
  onRemoveProjectWorkpath?: (node: WorkpathNode) => void;
  isProjectWorkpath?: boolean;
  batchMode?: boolean;
  batchSelectionState?: BatchSelectionState;
  onToggleBatchSelectionScope?: (scope: BatchSelectableScope) => void;
  renderEntry: (entry: SessionEntry) => React.ReactNode;
  displayPreferences?: SidebarDisplayPreferences;
  gitBranch?: string | null;
}

/**
 * First-level workpath drawer: header row (expand arrow + folder/home icon +
 * display name + session-count badge + hover ops) and, when expanded, up to two
 * SessionKindGroup sub-drawers (interactive first; empty groups not rendered).
 * Collapse interaction follows the WorkspaceCollapse paradigm (conditional
 * render, h-34px header, hover bg, trailing ops revealed on hover).
 */
const WorkpathDrawer: React.FC<WorkpathDrawerProps> = ({
  node,
  ui,
  activeSessionId,
  onCreateInteractive,
  onCreateTerminal,
  onRemoveProjectWorkpath,
  isProjectWorkpath = false,
  batchMode = false,
  batchSelectionState,
  onToggleBatchSelectionScope,
  renderEntry,
  displayPreferences = DEFAULT_SIDEBAR_DISPLAY_PREFERENCES,
  gitBranch,
}) => {
  const { t } = useTranslation();
  const [createMenuVisible, setCreateMenuVisible] = useState(false);
  const syncedActiveDrawerRouteRef = useRef<string | null>(null);
  const syncedActiveSubgroupRouteRef = useRef<string | null>(null);
  const [showAllSessionsByKind, setShowAllSessionsByKind] = useState<Record<SessionKind, boolean>>({
    interactive: false,
    terminal: false,
  });

  const isDefault = node.key === DEFAULT_WORKPATH_KEY;
  const displayName = isDefault ? t('sessionList.defaultWorkpath') : node.displayName;
  const workpathDisplay = isDefault ? null : formatWorkpathDisplay(node.key, node.displayName, displayPreferences.workpathNameMode);
  const twoLineWorkpath = workpathDisplay?.kind === 'twoLine';
  const sessionCount = node.interactive.length + node.terminal.length;

  const activeKind: SessionKind | null = activeSessionId
    ? node.interactive.some((entry) => entry.id === activeSessionId)
      ? 'interactive'
      : node.terminal.some((entry) => entry.id === activeSessionId)
        ? 'terminal'
        : null
    : null;
  const activeDisplayIndex =
    activeKind && activeSessionId !== null
      ? getWorkpathEntryDisplayIndex(node, { kind: activeKind, id: activeSessionId })
      : null;
  const forceShowAllForActiveKind: Record<SessionKind, boolean> = {
    interactive: activeKind === 'interactive' && activeDisplayIndex !== null && activeDisplayIndex >= WORKPATH_COLLAPSED_SESSION_LIMIT,
    terminal: activeKind === 'terminal' && activeDisplayIndex !== null && activeDisplayIndex >= WORKPATH_COLLAPSED_SESSION_LIMIT,
  };
  const visibleEntries = getVisibleWorkpathEntries(node, {
    interactive: showAllSessionsByKind.interactive || forceShowAllForActiveKind.interactive,
    terminal: showAllSessionsByKind.terminal || forceShowAllForActiveKind.terminal,
  });
  const activeRouteKey =
    activeKind !== null && activeSessionId !== null ? `${node.key}:${activeKind}:${activeSessionId}` : null;
  const drawerExpansion = getRenderedExpansionState({
    active: activeKind !== null,
    persistedExpanded: ui.isExpanded(node.key),
    activeRouteSynced: syncedActiveDrawerRouteRef.current === activeRouteKey,
  });
  const expanded = drawerExpansion.expanded;

  // Workpath-level capability: knowledge base. P2 临时点亮规则（组内任一成员
  // binding enabled）— Task 11 / P3 切到 workpath 级单次查询后由 hook 内部替换。
  const knowledgeLit = useWorkpathKnowledgeLit(node, expanded);
  const selectedState = batchSelectionState ?? {
    conversationIds: new Set<number>(),
    terminalIds: new Set<number>(),
  };
  const workpathSelectionScope = getWorkpathBatchSelectionScope(node);
  const workpathSelectionState = getBatchSelectionScopeState(workpathSelectionScope, selectedState);

  useEffect(() => {
    if (!activeRouteKey || activeKind === null) return;
    const subgroupExpansion = getRenderedExpansionState({
      active: true,
      persistedExpanded: ui.isSubgroupExpanded(node.key, activeKind),
      activeRouteSynced: syncedActiveSubgroupRouteRef.current === activeRouteKey,
    });

    if (drawerExpansion.shouldSyncExpanded) {
      syncedActiveDrawerRouteRef.current = activeRouteKey;
      ui.expand(node.key);
    }
    if (subgroupExpansion.shouldSyncExpanded) {
      syncedActiveSubgroupRouteRef.current = activeRouteKey;
      ui.expandSubgroup(node.key, activeKind);
    }
  }, [
    activeKind,
    activeRouteKey,
    drawerExpansion.shouldSyncExpanded,
    node.key,
    ui,
  ]);

  const toggleDrawer = () => {
    ui.toggleExpanded(node.key);
  };

  const toggleShowAllSessionsForKind = (kind: SessionKind) => {
    setShowAllSessionsByKind((value) => ({
      ...value,
      [kind]: !value[kind],
    }));
  };

  const renderKindGroup = (kind: SessionKind, entries: SessionEntry[], totalCount = entries.length) => {
    if (entries.length === 0) return null;
    const kindSelectionScope = getWorkpathBatchSelectionScope(node, kind);
    const kindSelectionState = getBatchSelectionScopeState(kindSelectionScope, selectedState);
    const kindMeta = visibleEntries.kindMeta[kind];
    const kindExpansion = getRenderedExpansionState({
      active: activeKind === kind,
      persistedExpanded: ui.isSubgroupExpanded(node.key, kind),
      activeRouteSynced: syncedActiveSubgroupRouteRef.current === activeRouteKey,
    });
    return (
      <SessionKindGroup
        kind={kind}
        entries={entries}
        totalCount={totalCount}
        expanded={kindExpansion.expanded}
        onToggle={() => ui.toggleSubgroup(node.key, kind)}
        onCreate={() => (kind === 'interactive' ? onCreateInteractive(node) : onCreateTerminal(node))}
        batchMode={batchMode}
        selectionChecked={kindSelectionState.checked}
        selectionIndeterminate={kindSelectionState.indeterminate}
        selectionDisabled={kindSelectionState.disabled}
        onToggleSelection={() => onToggleBatchSelectionScope?.(kindSelectionScope)}
        hasOverflow={kindMeta.hasOverflow && !forceShowAllForActiveKind[kind]}
        hiddenCount={kindMeta.hiddenCount}
        showAll={showAllSessionsByKind[kind]}
        onToggleShowAll={() => toggleShowAllSessionsForKind(kind)}
        renderEntry={renderEntry}
      />
    );
  };

  const headerIcon = isDefault ? (
    <Home theme='outline' size={16} fill='currentColor' className='line-height-0' />
  ) : expanded ? (
    <FolderOpen theme='outline' size={16} fill='currentColor' className='line-height-0' />
  ) : (
    <FolderClose theme='outline' size={16} fill='currentColor' className='line-height-0' />
  );

  const nameSpan = (
    <span className='text-14px font-[500] truncate text-t-primary min-w-0'>{displayName}</span>
  );
  const renderWorkpathName = () => {
    if (isDefault || !workpathDisplay) return nameSpan;
    if (workpathDisplay.kind === 'compressed') {
      return (
        <Tooltip content={node.key} position='top'>
          <span className='inline-flex min-w-0'>
            <PathText path={node.key} className='text-14px font-[500] text-t-primary' />
          </span>
        </Tooltip>
      );
    }
    if (workpathDisplay.kind === 'single') {
      return (
        <Tooltip content={workpathDisplay.tooltip} position='top'>
          <span className='text-14px font-[500] truncate text-t-primary min-w-0'>{workpathDisplay.primary}</span>
        </Tooltip>
      );
    }
    return (
      <Tooltip content={workpathDisplay.tooltip} position='top'>
        <span className='min-w-0 flex-1 flex flex-col justify-center overflow-hidden gap-2px'>
          <span className='text-13px font-[500] truncate text-t-primary leading-16px'>{workpathDisplay.primary}</span>
          {workpathDisplay.secondary && (
            <PathText path={workpathDisplay.secondary} className='text-11px font-[400] text-t-tertiary leading-13px' />
          )}
        </span>
      </Tooltip>
    );
  };

  const branchBadge =
    displayPreferences.showGitBranch && !isDefault && gitBranch ? (
      <Tooltip content={t('sessionList.currentGitBranch', { branch: gitBranch })} position='top'>
        <span className='shrink-0 max-w-78px h-18px px-5px rd-4px bg-fill-2 text-11px text-t-tertiary flex items-center gap-3px min-w-0'>
          <BranchOne theme='outline' size='11' fill='currentColor' className='shrink-0' />
          <span className='truncate min-w-0'>{gitBranch}</span>
        </span>
      </Tooltip>
    ) : null;

  return (
    <div className='workpath-drawer min-w-0'>
      {/* Drawer header */}
      <div
        className={classNames(
          'flex items-center gap-8px pl-10px pr-8px cursor-pointer hover:bg-fill-3 rd-8px transition-colors min-w-0 group',
          twoLineWorkpath ? 'h-42px py-4px' : 'h-34px'
        )}
        onClick={() => {
          if (batchMode && !workpathSelectionState.disabled) {
            onToggleBatchSelectionScope?.(workpathSelectionScope);
            return;
          }
          toggleDrawer();
        }}
      >
        {batchMode && (
          <span
            className='shrink-0 flex-center'
            onClick={(e) => {
              e.stopPropagation();
            }}
          >
            <Checkbox
              checked={workpathSelectionState.checked}
              indeterminate={workpathSelectionState.indeterminate}
              disabled={workpathSelectionState.disabled}
              onChange={() => onToggleBatchSelectionScope?.(workpathSelectionScope)}
            />
          </span>
        )}
        <span
          className='size-22px flex items-center justify-center shrink-0 text-t-primary'
          onClick={(e) => {
            e.stopPropagation();
            toggleDrawer();
          }}
        >
          {headerIcon}
        </span>

        <div className='flex-1 min-w-0 flex items-center gap-6px overflow-hidden'>
          {/* Workpath capability markers live with the identity text, not the
              hover action slot, so they never disappear under create/pin ops. */}
          {knowledgeLit && (
            <span className='shrink-0 flex items-center'>
              <CapabilityIcon
                icon={<BookOne theme='outline' size={13} fill='currentColor' />}
                color={CAPABILITY_COLORS.primary}
                title={t('knowledge.title')}
                size={13}
              />
            </span>
          )}

          {/* Default node shows its localized label; real workpaths follow the
              user's display preference, with the complete path still available
              from the tooltip and copy op beside it. */}
          {renderWorkpathName()}
          {branchBadge}
          {sessionCount > 0 && <span className='shrink-0 text-12px text-t-tertiary leading-none'>{sessionCount}</span>}
        </div>

        {/* Pinned dot indicator (rest state; hidden once hover ops appear) */}
        {!batchMode && node.pinned && (
          <span className={classNames('size-6px rd-full shrink-0 bg-aou-1', { 'group-hover:hidden': !createMenuVisible })} />
        )}

        {/* Hover ops: copy path + "+" create menu + pin toggle. */}
        {!batchMode && (
          <span
            className={classNames('shrink-0 items-center gap-6px', {
              flex: createMenuVisible,
              'hidden group-hover:flex': !createMenuVisible,
            })}
            onClick={(e) => e.stopPropagation()}
          >
            {!isDefault && (
              <CopyIconButton text={node.key} tooltip={t('common.copyPath')} className='shrink-0 size-20px sider-action-btn' />
            )}
            <Dropdown
              droplist={
                <Menu
                  onClickMenuItem={(menuKey) => {
                    setCreateMenuVisible(false);
                    if (menuKey === 'new-interactive') {
                      onCreateInteractive(node);
                    } else if (menuKey === 'new-terminal') {
                      onCreateTerminal(node);
                    }
                  }}
                >
                  <Menu.Item key='new-interactive'>
                    <div className='flex items-center gap-8px'>
                      <MessageOne theme='outline' size='14' />
                      <span>{t('sessionList.newInteractive')}</span>
                    </div>
                  </Menu.Item>
                  <Menu.Item key='new-terminal'>
                    <div className='flex items-center gap-8px'>
                      <Terminal theme='outline' size='14' />
                      <span>{t('sessionList.newTerminal')}</span>
                    </div>
                  </Menu.Item>
                </Menu>
              }
              trigger='click'
              position='br'
              popupVisible={createMenuVisible}
              onVisibleChange={setCreateMenuVisible}
              getPopupContainer={() => document.body}
              unmountOnExit={false}
            >
              <span
                role='button'
                tabIndex={0}
                aria-label={t('sessionList.create')}
                className='flex-center cursor-pointer transition-colors text-t-secondary hover:text-t-primary size-20px rd-4px sider-action-btn'
                onClick={(e) => {
                  e.stopPropagation();
                  setCreateMenuVisible((v) => !v);
                }}
              >
                <Plus theme='outline' size='14' fill='currentColor' className='block leading-none' />
              </span>
            </Dropdown>
            <Tooltip content={node.pinned ? t('sessionList.unpinWorkpath') : t('sessionList.pinWorkpath')} position='top'>
              <span
                role='button'
                tabIndex={0}
                aria-label={node.pinned ? t('sessionList.unpinWorkpath') : t('sessionList.pinWorkpath')}
                className={classNames(
                  'flex-center cursor-pointer transition-colors hover:text-t-primary size-20px rd-4px sider-action-btn',
                  node.pinned ? 'text-aou-1' : 'text-t-secondary'
                )}
                onClick={(e) => {
                  e.stopPropagation();
                  ui.togglePinned(node.key);
                }}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    e.stopPropagation();
                    ui.togglePinned(node.key);
                  }
                }}
              >
                <Pushpin theme='outline' size='14' fill='currentColor' className='block leading-none' />
              </span>
            </Tooltip>
            {!isDefault && isProjectWorkpath && onRemoveProjectWorkpath && (
              <Tooltip content={t('sessionList.removeWorkpath')} position='top'>
                <span
                  role='button'
                  tabIndex={0}
                  aria-label={t('sessionList.removeWorkpath')}
                  className='flex-center cursor-pointer transition-colors text-t-secondary hover:text-[rgb(var(--danger-6))] size-20px rd-4px sider-action-btn'
                  onClick={(e) => {
                    e.stopPropagation();
                    onRemoveProjectWorkpath(node);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      e.stopPropagation();
                      onRemoveProjectWorkpath(node);
                    }
                  }}
                >
                  <DeleteOne theme='outline' size='14' fill='currentColor' className='block leading-none' />
                </span>
              </Tooltip>
            )}
          </span>
        )}
      </div>

      {/* Drawer content: interactive subgroup first, then terminal; empty groups skipped. */}
      {expanded && (
        <div className='workpath-drawer-content min-w-0 flex flex-col'>
          {renderKindGroup('interactive', visibleEntries.interactive, node.interactive.length)}
          {renderKindGroup('terminal', visibleEntries.terminal, node.terminal.length)}
        </div>
      )}
    </div>
  );
};

export default WorkpathDrawer;
