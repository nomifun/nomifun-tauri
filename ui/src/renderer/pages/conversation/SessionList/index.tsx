/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { TChatConversation } from '@/common/config/storage';
import type { ConversationId, TerminalId } from '@/common/types/ids';
import BatchActionBar from '@/renderer/components/base/BatchActionBar';
import DirectorySelectionModal from '@/renderer/components/settings/DirectorySelectionModal';
import { useConversationHistoryContext } from '@/renderer/hooks/context/ConversationHistoryContext';
import { useCronJobsMap } from '@/renderer/pages/cron';
import { useTerminalSessions } from '@/renderer/pages/terminal/useTerminalSessions';
import { emitter } from '@/renderer/utils/emitter';
import { parseSessionRoute } from '@/renderer/utils/routes/sessionRoute';
import { scrollSidebarItemIntoView } from '@/renderer/utils/ui/scrollIntoView';
import { cleanupSiderTooltips } from '@/renderer/utils/ui/siderTooltip';
import { Empty, Input, Message, Modal } from '@arco-design/web-react';
import { FolderOpen } from '@icon-park/react';
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation, useNavigate } from 'react-router-dom';

import ConversationRow from './ConversationRow';
import CompanionSessionGroup from './CompanionSessionGroup';
import { useBatchSelection } from './hooks/useBatchSelection';
import { useConversationActions } from './hooks/useConversationActions';
import { useExport } from './hooks/useExport';
import { capabilityKey, useSessionCapabilities } from './hooks/useSessionCapabilities';
import { useWorkpathBranches } from './hooks/useWorkpathBranches';
import type { ConversationRowProps } from './types';
import TerminalRow from './TerminalRow';
import WorkpathDrawer from './WorkpathDrawer';
import { useWorkpathUiState } from './hooks/useWorkpathUiState';
import { toggleBatchSelectionScope, type BatchSelectableScope } from './utils/batchSelectionScopes';
import { DEFAULT_WORKPATH_KEY } from './utils/workpathKey';
import { buildWorkpathTree } from './utils/workpathTree';
import {
  getProjectWorkpaths,
  migrateProjectWorkpaths,
  removeProjectWorkpath,
  subscribeProjectWorkpaths,
} from './utils/projectWorkpaths';
import {
  DEFAULT_SIDEBAR_DISPLAY_PREFERENCES,
  type SidebarDisplayPreferences,
} from './utils/sidebarDisplayPreferences';
import type { SessionEntry, SessionKind, WorkpathNode } from './utils/workpathTree';

export type WorkpathSessionListProps = {
  onSessionClick?: () => void;
  collapsed?: boolean;
  tooltipEnabled?: boolean;
  batchMode?: boolean;
  displayPreferences?: SidebarDisplayPreferences;
  onBatchModeChange?: (value: boolean) => void;
};

/**
 * Unified workpath session list: first level groups every session (interactive
 * conversations + terminals) under its workpath drawer; second level splits by
 * session kind. Replaced GroupedHistory's 对话/置顶/项目 three-段式 plus the
 * standalone terminal section (mounted from the Sider scroll area).
 */
const WorkpathSessionList: React.FC<WorkpathSessionListProps> = ({
  onSessionClick,
  collapsed = false,
  tooltipEnabled = false,
  batchMode = false,
  displayPreferences = DEFAULT_SIDEBAR_DISPLAY_PREFERENCES,
  onBatchModeChange,
}) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { pathname } = useLocation();
  const {
    getJobStatus,
    markAsRead,
    setActiveConversation: setCronActiveConversation,
  } = useCronJobsMap();
  // AutoWork / IDMM enabled-state snapshot (bulk fetch + WS events, no per-row requests).
  const capabilities = useSessionCapabilities();

  const {
    conversations,
    isConversationGenerating,
    hasCompletionUnread,
    clearCompletionUnread,
    setActiveConversation,
  } = useConversationHistoryContext();
  const { sessions: terminals } = useTerminalSessions();
  const ui = useWorkpathUiState();
  const [emptyProjectWorkpaths, setEmptyProjectWorkpaths] = useState<string[]>(() => getProjectWorkpaths());

  useEffect(() => {
    return subscribeProjectWorkpaths(() => setEmptyProjectWorkpaths(getProjectWorkpaths()));
  }, []);

  const tree = useMemo(
    () => buildWorkpathTree(conversations, terminals, ui.pinnedKeys, emptyProjectWorkpaths),
    [conversations, terminals, ui.pinnedKeys, emptyProjectWorkpaths]
  );

  useEffect(() => {
    const inferredWorkpaths = tree
      .filter((node) => node.key !== DEFAULT_WORKPATH_KEY)
      .map((node) => node.key);
    if (inferredWorkpaths.length === 0) return;

    const migratedWorkpaths = migrateProjectWorkpaths(inferredWorkpaths);
    setEmptyProjectWorkpaths((current) => {
      if (current.length === migratedWorkpaths.length && current.every((path, index) => path === migratedWorkpaths[index])) {
        return current;
      }
      return migratedWorkpaths;
    });
  }, [tree]);

  const projectWorkpathKeys = useMemo(() => new Set(emptyProjectWorkpaths), [emptyProjectWorkpaths]);
  const branchWorkpaths = useMemo(
    () => tree.filter((node) => node.key !== DEFAULT_WORKPATH_KEY).map((node) => node.key),
    [tree]
  );
  const workpathBranches = useWorkpathBranches(branchWorkpaths, displayPreferences.showGitBranch && !collapsed);

  // Active session from the route — used for row selected state and drawer expansion.
  const activeRoute = useMemo(() => parseSessionRoute(pathname), [pathname]);
  const activeConversationId = activeRoute?.kind === 'conversation' ? activeRoute.id : null;
  const activeTerminalId = activeRoute?.kind === 'terminal' ? activeRoute.id : null;
  const activeSessionId: ConversationId | TerminalId | null = activeConversationId ?? activeTerminalId;

  // Sync active-conversation bookkeeping + scroll it into view on route change
  // (carried over from GroupedHistory / useConversations).
  useEffect(() => {
    if (!activeConversationId) {
      setActiveConversation(null);
      return;
    }
    setActiveConversation(activeConversationId);
    setCronActiveConversation(activeConversationId);
    clearCompletionUnread(activeConversationId);
    return scrollSidebarItemIntoView('c-' + activeConversationId);
  }, [activeConversationId, setActiveConversation, setCronActiveConversation, clearCompletionUnread]);

  /* ------------------------------- batch selection ------------------------------- */

  // All interactive sessions are selectable (the old "project members excluded"
  // rule is gone — projects no longer exist as a separate grouping).
  const {
    selectedConversationIds,
    setSelectedConversationIds,
    toggleSelectedConversation,
  } = useBatchSelection(batchMode, conversations);

  // Terminal selection — same semantics, kept locally (useTerminalBatchSelection
  // owns its own batchMode flag, which must stay unified with the prop here).
  const [selectedTerminalIds, setSelectedTerminalIds] = useState<Set<TerminalId>>(new Set());
  useEffect(() => {
    if (!batchMode) setSelectedTerminalIds(new Set());
  }, [batchMode]);
  useEffect(() => {
    if (!batchMode || selectedTerminalIds.size === 0) return;
    const existing = new Set(terminals.map((session) => session.id));
    setSelectedTerminalIds((prev) => {
      const next = new Set(Array.from(prev).filter((id) => existing.has(id)));
      return next.size === prev.size ? prev : next;
    });
  }, [batchMode, terminals, selectedTerminalIds.size]);

  const toggleSelectedTerminal = useCallback((id: TerminalId) => {
    setSelectedTerminalIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const totalSelected = selectedConversationIds.size + selectedTerminalIds.size;
  const totalSelectable = conversations.length + terminals.length;
  const allSelected = totalSelectable > 0 && totalSelected === totalSelectable;
  const batchSelectionState = useMemo(
    () => ({
      conversationIds: selectedConversationIds,
      terminalIds: selectedTerminalIds,
    }),
    [selectedConversationIds, selectedTerminalIds]
  );
  const handleToggleSelectAll = useCallback(() => {
    if (allSelected) {
      setSelectedConversationIds(new Set());
      setSelectedTerminalIds(new Set());
    } else {
      setSelectedConversationIds(new Set(conversations.map((conversation) => conversation.id)));
      setSelectedTerminalIds(new Set(terminals.map((session) => session.id)));
    }
  }, [allSelected, conversations, terminals, setSelectedConversationIds]);
  const handleToggleBatchSelectionScope = useCallback(
    (scope: BatchSelectableScope) => {
      const next = toggleBatchSelectionScope(scope, batchSelectionState);
      setSelectedConversationIds(next.conversationIds);
      setSelectedTerminalIds(next.terminalIds);
    },
    [batchSelectionState, setSelectedConversationIds]
  );

  /* --------------------------- per-row actions & modals --------------------------- */

  const {
    renameModalVisible,
    renameModalName,
    setRenameModalName,
    renameLoading,
    dropdownVisibleId,
    handleConversationClick,
    handleDeleteClick,
    handleEditStart,
    handleRenameConfirm,
    handleRenameCancel,
    handleTogglePin,
    handleMenuVisibleChange,
    handleOpenMenu,
  } = useConversationActions({
    activeConversationId,
    batchMode,
    onSessionClick,
    onBatchModeChange,
    selectedConversationIds,
    setSelectedConversationIds,
    toggleSelectedConversation,
    markAsRead,
  });

  const {
    exportTask,
    exportModalVisible,
    exportTargetPath,
    exportModalLoading,
    showExportDirectorySelector,
    setShowExportDirectorySelector,
    closeExportModal,
    handleSelectExportDirectoryFromModal,
    handleSelectExportFolder,
    handleExportConversation,
    handleBatchExport,
    handleConfirmExport,
  } = useExport({
    conversations,
    selectedConversationIds,
    setSelectedConversationIds,
    onBatchModeChange,
  });

  // Combined batch delete spanning both kinds. Conversation removal mirrors
  // useConversationActions.removeConversation (event emit + navigate-away);
  // terminal removal was carried over from the since-removed TerminalSiderSection /
  // useTerminalBatchSelection (best-effort per id).
  const handleBatchDeleteAll = useCallback(() => {
    const convIds = Array.from(selectedConversationIds);
    const termIds = Array.from(selectedTerminalIds);
    const total = convIds.length + termIds.length;
    if (total === 0) {
      Message.warning(t('conversation.history.batchNoSelection'));
      return;
    }
    Modal.confirm({
      title: t('conversation.history.batchDelete', { count: total }),
      content: t('conversation.history.batchDeleteConfirm', { count: total }),
      okText: t('conversation.history.confirmDelete'),
      cancelText: t('conversation.history.cancelDelete'),
      okButtonProps: { status: 'warning' },
      onOk: async () => {
        let successCount = 0;
        try {
          const convResults = await Promise.all(
            convIds.map(async (conversation_id) => {
              try {
                const success = await ipcBridge.conversation.remove.invoke({ id: conversation_id });
                if (success) {
                  // conversation.deleted is a string-keyed event-bus channel; serialize the id.
                  emitter.emit('conversation.deleted', String(conversation_id));
                  if (activeConversationId === conversation_id) void navigate('/guid');
                }
                return success;
              } catch {
                return false;
              }
            })
          );
          successCount += convResults.filter(Boolean).length;
          for (const terminal_id of termIds) {
            try {
              await ipcBridge.terminal.remove.invoke({ id: terminal_id });
              if (activeTerminalId === terminal_id) void navigate('/guid');
              successCount += 1;
            } catch {
              /* best-effort; continue */
            }
          }
          emitter.emit('chat.history.refresh');
          if (successCount > 0) {
            Message.success(t('conversation.history.batchDeleteSuccess', { count: successCount }));
          } else {
            Message.error(t('conversation.history.deleteFailed'));
          }
        } finally {
          setSelectedConversationIds(new Set());
          setSelectedTerminalIds(new Set());
          onBatchModeChange?.(false);
        }
      },
      style: { borderRadius: '12px' },
      alignCenter: true,
      getPopupContainer: () => document.body,
    });
  }, [
    selectedConversationIds,
    selectedTerminalIds,
    activeConversationId,
    activeTerminalId,
    navigate,
    onBatchModeChange,
    setSelectedConversationIds,
    t,
  ]);

  /* ----------------------------- create-session entries ----------------------------- */

  const handleCreateInteractive = useCallback(
    (node: WorkpathNode) => {
      // default 节点不带 state —— 走普通「新建对话」流程
      if (node.key === DEFAULT_WORKPATH_KEY) {
        void navigate('/guid');
      } else {
        void navigate('/guid', { state: { workspace: node.key } });
      }
      onSessionClick?.();
    },
    [navigate, onSessionClick]
  );

  const handleCreateTerminal = useCallback(
    (node: WorkpathNode) => {
      if (node.key === DEFAULT_WORKPATH_KEY) {
        void navigate('/terminal-new');
      } else {
        void navigate('/terminal-new', { state: { cwd: node.key } });
      }
      onSessionClick?.();
    },
    [navigate, onSessionClick]
  );

  const handleRemoveProjectWorkpath = useCallback(
    (node: WorkpathNode) => {
      if (node.key === DEFAULT_WORKPATH_KEY) return;
      const sessionCount = node.interactive.length + node.terminal.length;
      Modal.confirm({
        title: t('sessionList.removeWorkpathTitle'),
        content: t(
          sessionCount > 0
            ? 'sessionList.removeWorkpathWithSessionsConfirm'
            : 'sessionList.removeWorkpathConfirm',
          { path: node.key, count: sessionCount }
        ),
        okText: t('common.remove'),
        cancelText: t('common.cancel'),
        okButtonProps: { status: 'danger' },
        onOk: () => {
          removeProjectWorkpath(node.key);
          if (node.pinned) ui.togglePinned(node.key);
          setEmptyProjectWorkpaths(getProjectWorkpaths());
          Message.success(t('sessionList.removeWorkpathSuccess'));
        },
        style: { borderRadius: '12px' },
        alignCenter: true,
        getPopupContainer: () => document.body,
      });
    },
    [t, ui]
  );

  /* ------------------------------- reveal on create ------------------------------- */

  // Auto-expand the owning drawer + kind subgroup and scroll a newly created
  // session into view (adapted from Sider/useRevealOnCreate: event-driven, never
  // fires on initial load; reveal waits until the row lands in the tree because
  // the workpath is only known after aggregation). Last-one-wins on bursts.
  const pendingRevealRef = useRef<{ kind: SessionKind; id: ConversationId | TerminalId } | null>(null);
  const [revealTick, setRevealTick] = useState(0);

  useEffect(() => {
    const offConversationCreated = ipcBridge.conversation.listChanged.on((event) => {
      if (event.action !== 'created') return;
      pendingRevealRef.current = { kind: 'interactive', id: event.conversation_id };
      setRevealTick((tick) => tick + 1);
    });
    const offTerminalCreated = ipcBridge.terminal.onCreated.on((session) => {
      pendingRevealRef.current = { kind: 'terminal', id: session.id };
      setRevealTick((tick) => tick + 1);
    });
    // TODO(cron): cron.job-created creates a *job*, not a session — its derived
    // conversation surfaces through conversation.listChanged 'created' and is
    // covered above. Revealing the anchored session of a job bound to an
    // EXISTING conversation/terminal is deferred to the cron-integration task.
    return () => {
      offConversationCreated();
      offTerminalCreated();
    };
  }, []);

  const { expand: expandWorkpathDrawer, expandSubgroup: expandWorkpathSubgroup } = ui;
  useEffect(() => {
    const pending = pendingRevealRef.current;
    if (!pending) return;
    const node = tree.find((candidate) =>
      pending.kind === 'interactive'
        ? candidate.interactive.some((entry) => entry.id === pending.id)
        : candidate.terminal.some((entry) => entry.id === pending.id)
    );
    if (!node) return; // not aggregated yet (async list refresh) — retry on next data change
    pendingRevealRef.current = null;
    expandWorkpathDrawer(node.key);
    expandWorkpathSubgroup(node.key, pending.kind);
    scrollSidebarItemIntoView(pending.kind === 'interactive' ? 'c-' + pending.id : 'terminal-' + pending.id);
  }, [tree, revealTick, expandWorkpathDrawer, expandWorkpathSubgroup]);

  /* ---------------------------------- row render ---------------------------------- */

  const getConversationRowProps = useCallback(
    (conversation: TChatConversation): ConversationRowProps => ({
      conversation,
      isGenerating: isConversationGenerating(conversation.id),
      hasCompletionUnread: hasCompletionUnread(conversation.id),
      collapsed,
      tooltipEnabled,
      batchMode,
      checked: selectedConversationIds.has(conversation.id),
      selected: activeConversationId === conversation.id,
      menuVisible: dropdownVisibleId !== null && dropdownVisibleId === conversation.id,
      onToggleChecked: toggleSelectedConversation,
      onConversationClick: handleConversationClick,
      onOpenMenu: handleOpenMenu,
      onMenuVisibleChange: handleMenuVisibleChange,
      onEditStart: handleEditStart,
      onDelete: handleDeleteClick,
      onExport: handleExportConversation,
      onTogglePin: handleTogglePin,
      getJobStatus,
      autoworkState: capabilities.autowork.get(capabilityKey('conversation', conversation.id)),
      idmmState: capabilities.idmm.get(capabilityKey('conversation', conversation.id)),
      showSessionAge: displayPreferences.sessionMetaMode === 'age',
    }),
    [
      collapsed,
      tooltipEnabled,
      batchMode,
      isConversationGenerating,
      hasCompletionUnread,
      selectedConversationIds,
      activeConversationId,
      dropdownVisibleId,
      toggleSelectedConversation,
      handleConversationClick,
      handleOpenMenu,
      handleMenuVisibleChange,
      handleEditStart,
      handleDeleteClick,
      handleExportConversation,
      handleTogglePin,
      getJobStatus,
      capabilities,
      displayPreferences.sessionMetaMode,
    ]
  );

  const handleTerminalClick = useCallback(
    (terminal_id: TerminalId) => {
      cleanupSiderTooltips();
      void navigate(`/terminal/${terminal_id}`);
      onSessionClick?.();
    },
    [navigate, onSessionClick]
  );

  const renderEntry = useCallback(
    (entry: SessionEntry): React.ReactNode => {
      if (entry.kind === 'interactive' && entry.conversation) {
        return <ConversationRow key={entry.id} {...getConversationRowProps(entry.conversation)} dimIcon />;
      }
      if (entry.kind === 'terminal' && entry.terminal) {
        return (
          <TerminalRow
            key={entry.id}
            session={entry.terminal}
            active={activeTerminalId === entry.id}
            onClick={() => handleTerminalClick(entry.id)}
            selectionMode={batchMode}
            selected={selectedTerminalIds.has(entry.id)}
            onToggleSelect={() => toggleSelectedTerminal(entry.id)}
            indent
            autoworkState={capabilities.autowork.get(capabilityKey('terminal', entry.id))}
            idmmState={capabilities.idmm.get(capabilityKey('terminal', entry.id))}
            showSessionAge={displayPreferences.sessionMetaMode === 'age'}
          />
        );
      }
      return null;
    },
    [
      getConversationRowProps,
      activeTerminalId,
      handleTerminalClick,
      batchMode,
      selectedTerminalIds,
      toggleSelectedTerminal,
      capabilities,
      displayPreferences.sessionMetaMode,
    ]
  );

  /* ------------------------------------ render ------------------------------------ */

  const modals = (
    <>
      {/* Rename modal (carried over from GroupedHistory) */}
      <Modal
        title={t('conversation.history.renameTitle')}
        visible={renameModalVisible}
        onOk={handleRenameConfirm}
        onCancel={handleRenameCancel}
        okText={t('conversation.history.saveName')}
        cancelText={t('conversation.history.cancelEdit')}
        confirmLoading={renameLoading}
        okButtonProps={{ disabled: !renameModalName.trim() }}
        style={{ borderRadius: '12px' }}
        alignCenter
        getPopupContainer={() => document.body}
      >
        <Input
          autoFocus
          value={renameModalName}
          onChange={setRenameModalName}
          onPressEnter={handleRenameConfirm}
          placeholder={t('conversation.history.renamePlaceholder')}
          allowClear
        />
      </Modal>

      {/* Export modal (carried over from GroupedHistory) */}
      <Modal
        visible={exportModalVisible}
        title={t('conversation.history.exportDialogTitle')}
        onCancel={closeExportModal}
        footer={null}
        style={{ borderRadius: '12px' }}
        className='conversation-export-modal'
        alignCenter
        getPopupContainer={() => document.body}
      >
        <div className='py-8px'>
          <div className='text-14px mb-16px text-t-secondary'>
            {exportTask?.mode === 'batch'
              ? t('conversation.history.exportDialogBatchDescription', { count: exportTask.conversation_ids.length })
              : t('conversation.history.exportDialogSingleDescription')}
          </div>

          <div className='mb-16px p-16px rounded-12px bg-fill-1'>
            <div className='text-14px mb-8px text-t-primary'>{t('conversation.history.exportTargetFolder')}</div>
            <div
              className='flex items-center justify-between px-12px py-10px rounded-8px transition-colors'
              style={{
                backgroundColor: 'var(--color-bg-1)',
                border: '1px solid var(--color-border-2)',
                cursor: exportModalLoading ? 'not-allowed' : 'pointer',
                opacity: exportModalLoading ? 0.55 : 1,
              }}
              onClick={() => {
                void handleSelectExportFolder();
              }}
            >
              <span
                className='text-14px overflow-hidden text-ellipsis whitespace-nowrap'
                style={{ color: exportTargetPath ? 'var(--color-text-1)' : 'var(--color-text-3)' }}
              >
                {exportTargetPath || t('conversation.history.exportSelectFolder')}
              </span>
              <FolderOpen theme='outline' size='18' fill='var(--color-text-3)' />
            </div>
          </div>

          <div className='flex items-center gap-8px mb-20px text-14px text-t-secondary'>
            <span>💡</span>
            <span>{t('conversation.history.exportDialogHint')}</span>
          </div>

          <div className='flex gap-12px justify-end'>
            <button
              className='px-24px py-8px rounded-20px text-14px font-medium transition-all'
              style={{
                border: '1px solid var(--color-border-2)',
                backgroundColor: 'var(--color-fill-2)',
                color: 'var(--color-text-1)',
              }}
              onMouseEnter={(event) => {
                event.currentTarget.style.backgroundColor = 'var(--color-fill-3)';
              }}
              onMouseLeave={(event) => {
                event.currentTarget.style.backgroundColor = 'var(--color-fill-2)';
              }}
              onClick={closeExportModal}
            >
              {t('common.cancel')}
            </button>
            <button
              className='px-24px py-8px rounded-20px text-14px font-medium transition-all'
              style={{
                border: 'none',
                backgroundColor: exportModalLoading ? 'var(--color-fill-3)' : 'var(--color-text-1)',
                color: 'var(--color-bg-1)',
                cursor: exportModalLoading ? 'not-allowed' : 'pointer',
              }}
              onMouseEnter={(event) => {
                if (!exportModalLoading) {
                  event.currentTarget.style.opacity = '0.85';
                }
              }}
              onMouseLeave={(event) => {
                if (!exportModalLoading) {
                  event.currentTarget.style.opacity = '1';
                }
              }}
              onClick={() => {
                void handleConfirmExport();
              }}
              disabled={exportModalLoading}
            >
              {exportModalLoading ? t('conversation.history.exporting') : t('common.confirm')}
            </button>
          </div>
        </div>
      </Modal>

      <DirectorySelectionModal
        visible={showExportDirectorySelector}
        onConfirm={handleSelectExportDirectoryFromModal}
        onCancel={() => setShowExportDirectorySelector(false)}
      />
    </>
  );

  // Collapsed sider: flat icon-only conversation rows (terminals were never
  // shown in the collapsed sider — same as the old TerminalSiderSection gating).
  if (collapsed) {
    return (
      <>
        {modals}
        <div className='min-w-0'>
          <CompanionSessionGroup
            collapsed
            activeConversationId={activeConversationId}
            onSessionClick={onSessionClick}
          />
          {tree.flatMap((node) =>
            node.interactive.map((entry) =>
              entry.conversation ? (
                <ConversationRow key={entry.id} {...getConversationRowProps(entry.conversation)} />
              ) : null
            )
          )}
        </div>
      </>
    );
  }

  const isEmpty = conversations.length === 0 && terminals.length === 0;

  return (
    <>
      {modals}
      <div className='min-w-0'>
        {/* 桌面伙伴专属工作空间分组（roster-driven，置于项目/工作路径之上）。仅交互式、
            不在此新建；可折叠（状态持久化于 useWorkpathUiState，默认展开）；
            点击伙伴行跳转其唯一会话 /conversation/:id。 */}
        <CompanionSessionGroup
          activeConversationId={activeConversationId}
          onSessionClick={onSessionClick}
          expanded={ui.companionGroupExpanded}
          onToggleExpanded={ui.toggleCompanionGroup}
        />

        <div data-testid='workpath-section-toolbar' className='px-2px pb-6px'>
          <div className='h-22px px-2px flex items-center justify-between gap-8px select-none'>
            <span className='text-13px text-t-tertiary font-[500] leading-none tracking-wide truncate'>
              {t('sessionList.workpathSection')}
            </span>
            {totalSelectable > 0 && (
              <span className='text-12px text-t-tertiary leading-none shrink-0'>{totalSelectable}</span>
            )}
          </div>
        </div>

        {tree.map((node) => (
          <WorkpathDrawer
            key={node.key}
            node={node}
            ui={ui}
            activeSessionId={activeSessionId}
            onCreateInteractive={handleCreateInteractive}
            onCreateTerminal={handleCreateTerminal}
            onRemoveProjectWorkpath={handleRemoveProjectWorkpath}
            isProjectWorkpath={projectWorkpathKeys.has(node.key)}
            batchMode={batchMode}
            batchSelectionState={batchSelectionState}
            onToggleBatchSelectionScope={handleToggleBatchSelectionScope}
            renderEntry={renderEntry}
            displayPreferences={displayPreferences}
            gitBranch={workpathBranches.get(node.key)}
          />
        ))}

        {/* 空态：default 抽屉常显（tree 恒含 default 节点）+ 新建引导文案 */}
        {isEmpty && (
          <div className='py-32px flex-center'>
            <Empty description={t('sessionList.empty')} />
          </div>
        )}

        {/* Batch action bar — spans both session kinds */}
        {batchMode && (
          <BatchActionBar
            selectAllLabel={allSelected ? t('common.cancel') : t('conversation.history.selectAll')}
            onSelectAll={handleToggleSelectAll}
            actions={[
              {
                key: 'export',
                label: t('conversation.history.batchExport', { count: selectedConversationIds.size }),
                onClick: handleBatchExport,
                disabled: selectedConversationIds.size === 0,
              },
              {
                key: 'delete',
                label: t('conversation.history.batchDelete', { count: totalSelected }),
                onClick: handleBatchDeleteAll,
                danger: true,
                disabled: totalSelected === 0,
              },
            ]}
          />
        )}
      </div>
    </>
  );
};

export default WorkpathSessionList;
export { WorkpathSessionList };
