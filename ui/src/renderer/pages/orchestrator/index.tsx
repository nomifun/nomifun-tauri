/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { Input, Popconfirm, Spin } from '@arco-design/web-react';
import { Comment, Delete, Edit, Plus, Right } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TRun } from '@/common/types/orchestrator/orchestratorTypes';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useContentSiderCollapse } from '@renderer/components/layout/ContentSider';
import {
  SESSION_SIDER_TOGGLE_EVENT,
  dispatchSessionSiderStateEvent,
} from '@renderer/utils/workspace/sessionSiderEvents';
import { dispatchWorkspaceAvailabilityEvent } from '@renderer/utils/workspace/workspaceEvents';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import RunHistory from './RunHistory';
import NewRunComposer, { type ReplanInitial } from './NewRunComposer';
import NewRunIntentBox from './NewRunIntentBox';
import WorkerTranscriptPanel from './RunDetail/WorkerTranscriptPanel';
import MobileRunSummary from './RunDetail/MobileRunSummary';
import RunView from './RunDetail/RunView';
import type { OpenTaskPayload } from './RunDetail/DagCanvas';
import { useMyRuns } from './useOrchestratorData';
import { useRunLive } from './useRunLive';

/** Run status → theme-var color + i18n label key suffix (mirrors RunHistory). */
const STATUS_META: Record<string, { color: string; key: string }> = {
  planning: { color: 'var(--warning)', key: 'planning' },
  running: { color: 'rgb(var(--primary-6))', key: 'running' },
  completed: { color: 'var(--success)', key: 'completed' },
  failed: { color: 'var(--danger)', key: 'failed' },
  cancelled: { color: 'var(--color-text-3)', key: 'cancelled' },
  paused: { color: 'var(--warning)', key: 'paused' },
  awaiting_plan_approval: { color: 'var(--warning)', key: 'awaiting_plan_approval' },
};

const formatTime = (ms: number): string => new Date(ms).toLocaleString();

/**
 * A single run row in the left master list. Reuses RunHistory's visual language
 * (goal · status dot · timestamp), with a selected highlight, an optional "open
 * conversation" jump, and hover-revealed management actions (rename / delete).
 * Rename swaps the goal line for an inline input; delete is confirm-guarded.
 */
const RunListRow: React.FC<{
  run: TRun;
  selected: boolean;
  onSelect: () => void;
  onOpenConversation?: () => void;
  onRename: (goal: string) => Promise<void>;
  onDelete: () => Promise<void>;
}> = ({ run, selected, onSelect, onOpenConversation, onRename, onDelete }) => {
  const { t } = useTranslation();
  const meta = STATUS_META[run.status];
  const dotColor = meta?.color ?? 'var(--color-text-3)';
  const statusLabel = t(`orchestrator.run.status.${meta?.key ?? 'unknown'}`);
  const goalText = run.goal.trim() || t('orchestrator.run.untitledGoal');

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(run.goal);
  const [saving, setSaving] = useState(false);

  const beginEdit = useCallback(() => {
    setDraft(run.goal);
    setEditing(true);
  }, [run.goal]);

  const commitEdit = useCallback(async () => {
    const next = draft.trim();
    if (!next || next === run.goal.trim()) {
      setEditing(false);
      return;
    }
    setSaving(true);
    try {
      await onRename(next);
      setEditing(false);
    } finally {
      setSaving(false);
    }
  }, [draft, run.goal, onRename]);

  // Inline rename mode — replaces the whole row with an input so the user can
  // retitle without leaving the list. Enter commits, Escape/blur cancels.
  if (editing) {
    return (
      <div className='flex items-center gap-8px rd-10px px-12px py-6px' style={{ border: '1px solid rgb(var(--primary-6))' }}>
        <Input
          autoFocus
          size='small'
          disabled={saving}
          value={draft}
          onChange={setDraft}
          onPressEnter={() => void commitEdit()}
          onBlur={() => void commitEdit()}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.preventDefault();
              setEditing(false);
            }
          }}
          className='flex-1'
        />
      </div>
    );
  }

  return (
    <div
      role='button'
      tabIndex={0}
      aria-pressed={selected}
      onClick={onSelect}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onSelect();
        }
      }}
      className='group flex cursor-pointer select-none items-center gap-8px rd-10px px-12px py-10px transition-all duration-150'
      style={{
        background: selected ? 'color-mix(in srgb, rgb(var(--primary-6)) 8%, var(--bg-2))' : 'transparent',
        border: `1px solid ${selected ? 'rgb(var(--primary-6))' : 'transparent'}`,
        boxShadow: selected ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 16%, transparent)' : undefined,
      }}
    >
      <span
        className='size-7px shrink-0 rd-full'
        style={{ background: dotColor, boxShadow: `0 0 0 2px color-mix(in srgb, ${dotColor} 22%, transparent)` }}
      />
      <div className='min-w-0 flex-1'>
        <div className='truncate text-13px font-600 leading-tight text-t-primary'>{goalText}</div>
        <div className='mt-3px flex items-center gap-6px truncate text-11px text-t-tertiary'>
          <span className='shrink-0' style={{ color: dotColor }}>
            {statusLabel}
          </span>
          <span className='shrink-0'>·</span>
          <span className='truncate'>{formatTime(run.created_at)}</span>
        </div>
      </div>

      {/* Hover actions: open conversation · rename · delete. */}
      {onOpenConversation && (
        <RowAction
          label={t('orchestrator.run.openConversation')}
          onClick={onOpenConversation}
          hoverClass='hover:bg-fill-2 hover:text-primary-6'
        >
          <Comment theme='outline' size='14' strokeWidth={3} />
        </RowAction>
      )}
      <RowAction
        label={t('orchestrator.run.manage.rename')}
        onClick={beginEdit}
        hoverClass='hover:bg-fill-2 hover:text-primary-6'
      >
        <Edit theme='outline' size='14' strokeWidth={3} />
      </RowAction>
      <Popconfirm
        focusLock
        title={t('orchestrator.run.manage.deleteConfirm')}
        okText={t('orchestrator.run.manage.deleteConfirmOk')}
        cancelText={t('orchestrator.run.manage.deleteConfirmCancel')}
        onOk={() => void onDelete()}
      >
        <RowAction
          label={t('orchestrator.run.manage.delete')}
          onClick={(e) => e.stopPropagation()}
          hoverClass='hover:bg-danger-light-1 hover:text-danger'
        >
          <Delete theme='outline' size='14' strokeWidth={3} />
        </RowAction>
      </Popconfirm>
      <Right theme='outline' size='14' strokeWidth={3} className='shrink-0 text-t-tertiary group-hover:hidden' />
    </div>
  );
};

/** A small hover-revealed icon action inside a run row. Stops click propagation
 * so activating it never also selects the row (unless the handler opts in). */
const RowAction: React.FC<{
  label: string;
  hoverClass: string;
  onClick: (e: React.MouseEvent) => void;
  children: React.ReactNode;
}> = ({ label, hoverClass, onClick, children }) => (
  <div
    role='button'
    tabIndex={0}
    aria-label={label}
    title={label}
    onClick={(e) => {
      e.stopPropagation();
      onClick(e);
    }}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        e.stopPropagation();
        onClick(e as unknown as React.MouseEvent);
      }
    }}
    className={`flex size-26px shrink-0 items-center justify-center rd-6px text-t-tertiary opacity-0 transition-all group-hover:opacity-100 ${hoverClass}`}
  >
    {children}
  </div>
);

/**
 * localStorage key for the run-list rail collapse preference. Owned by the page
 * via {@link useContentSiderCollapse}; the titlebar's session-sider toggle drives
 * it over the shared {@link SESSION_SIDER_TOGGLE_EVENT} bus (an orchestrator-
 * specific key so the run-list and the conversation session list never bleed
 * collapse state into each other).
 */
const RUNLIST_COLLAPSE_KEY = 'nomifun:orchestrator-runlist-collapsed';

/**
 * RunListRail — the master column: a prominent 「＋ 新建 Run」button atop a
 * scrollable list of the current user's runs (active + history, newest first via
 * {@link useMyRuns}). Each row carries rename / delete management actions.
 * Selecting a row is the page's primary navigation; the 「新建」button and any
 * open run live in the detail pane on the right.
 *
 * Collapse is owned by {@link OrchestratorPage} (titlebar session-sider toggle):
 * when collapsed the page simply does not render this rail — mirroring the
 * conversation Tab's {@link ConversationShell}, which hides its session list the
 * same way. So this component no longer carries an in-panel collapse button.
 */
const RunListRail: React.FC<{
  selectedRunId: string | undefined;
  newRunActive: boolean;
  onNewRun: () => void;
  onSelectRun: (id: string) => void;
  onRunDeleted: (id: string) => void;
}> = ({ selectedRunId, newRunActive, onNewRun, onSelectRun, onRunDeleted }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { runs, isLoading, error, mutate } = useMyRuns();
  const [message, msgCtx] = useArcoMessage();

  const handleRename = useCallback(
    async (id: string, goal: string) => {
      try {
        await ipcBridge.orchestrator.runs.rename.invoke({ id, goal });
        mutate();
      } catch (e) {
        message.error(t('orchestrator.run.manage.renameError', { error: String(e) }));
      }
    },
    [mutate, message, t]
  );

  const handleDelete = useCallback(
    async (id: string) => {
      try {
        await ipcBridge.orchestrator.runs.remove.invoke({ id });
        mutate();
        onRunDeleted(id);
        message.success(t('orchestrator.run.manage.deleteOk'));
      } catch (e) {
        message.error(t('orchestrator.run.manage.deleteError', { error: String(e) }));
      }
    },
    [mutate, onRunDeleted, message, t]
  );

  return (
    <div className='flex h-full min-h-0 w-300px shrink-0 flex-col border-r border-r-base bg-1'>
      {msgCtx}
      {/* Header + new-run button */}
      <div className='shrink-0 px-16px pt-16px pb-12px'>
        <div className='min-w-0'>
          <div className='text-15px font-600 leading-tight text-t-primary'>{t('orchestrator.title')}</div>
          <div className='mt-2px text-11px leading-15px text-t-tertiary'>{t('orchestrator.subtitle')}</div>
        </div>
        <div
          role='button'
          tabIndex={0}
          aria-pressed={newRunActive}
          onClick={onNewRun}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              onNewRun();
            }
          }}
          className='mt-12px flex h-36px cursor-pointer select-none items-center justify-center gap-6px rd-9px text-13px font-500 text-white transition-opacity hover:opacity-90'
          style={{
            background: 'rgb(var(--primary-6))',
            boxShadow: newRunActive ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 22%, transparent)' : undefined,
          }}
        >
          <Plus theme='outline' size='15' strokeWidth={4} />
          <span>{t('orchestrator.tab.newRun')}</span>
        </div>
      </div>

      {/* List header */}
      <div className='shrink-0 px-16px pb-6px text-11px font-600 uppercase leading-none tracking-wide text-t-tertiary'>
        {t('orchestrator.tab.listTitle')}
      </div>

      {/* Scrollable run list */}
      <div className='min-h-0 flex-1 overflow-y-auto px-8px pb-12px'>
        {isLoading ? (
          <div className='flex items-center justify-center py-32px'>
            <Spin />
          </div>
        ) : error ? (
          <div className='px-8px py-24px text-center text-12px text-t-tertiary'>
            {t('orchestrator.tab.listLoadError')}
          </div>
        ) : runs.length === 0 ? (
          <div className='px-8px py-24px text-center text-12px leading-18px text-t-tertiary'>
            {t('orchestrator.tab.listEmpty')}
          </div>
        ) : (
          <div className='flex flex-col gap-2px'>
            {runs.map((run) => (
              <RunListRow
                key={run.id}
                run={run}
                selected={!newRunActive && selectedRunId === run.id}
                onSelect={() => onSelectRun(run.id)}
                onOpenConversation={
                  run.lead_conv_id != null ? () => void navigate(`/conversation/${run.lead_conv_id}`) : undefined
                }
                onRename={(goal) => handleRename(run.id, goal)}
                onDelete={() => handleDelete(run.id)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
};

/**
 * OrchestratorPage (/orchestrator) — 「智能编排」(orchestration), rebuilt as a
 * master-detail workspace. The left rail ({@link RunListRail}) lists the user's
 * runs with a prominent 「＋ 新建 Run」button; the detail pane has three states:
 *
 *  1. **composing** — the full structured {@link NewRunComposer} (reached from the
 *     intent box's 「结构化新建」link for advanced control). On `onCreated` we
 *     select the new run; on `onCancel` we drop back to the intent box.
 *  2. **a run selected** (`?run=<id>`) — the run view: an {@link AgentRoster}
 *     strip atop the interactive {@link DagCanvas} (which itself renders the
 *     run-detail header + status-aware controls + the completed-run role
 *     precipitation panel, and wires cancel/approve/pause/resume internally —
 *     so we don't duplicate those here). Clicking a roster card or a DAG node
 *     opens the {@link WorkerTranscriptPanel} drawer.
 *  3. **nothing selected** — the conversational {@link NewRunIntentBox}: type a
 *     natural-language intent → a fresh ad-hoc run is planned & selected. This is
 *     the quick path; 「＋ 新建 Run」simply lands here.
 *
 * `?run=<id>` is kept in the URL (browser-back closes a run / a deep-link
 * selects one on mount). On mobile the interactive canvas is too awkward, so a
 * read-only {@link MobileRunSummary} (and the {@link RunHistory} list) is shown.
 */
const OrchestratorPage: React.FC = () => {
  const { t } = useTranslation();
  const layout = useLayoutContext();
  const isMobile = layout?.isMobile ?? false;
  const [searchParams, setSearchParams] = useSearchParams();

  // ── Master-detail state ────────────────────────────────────────────────────
  // `?run=<id>` is the source of truth for the selected run (deep-link + back).
  const runParam = searchParams.get('run');
  const selectedRunId = runParam && runParam !== '' ? runParam : undefined;

  const [composing, setComposing] = useState(false);
  // In-place re-plan editor for the selected run (overlays the run view).
  const [replanning, setReplanning] = useState(false);
  // The clicked DAG node / roster card payload → opens the transcript drawer.
  const [selectedTask, setSelectedTask] = useState<OpenTaskPayload | null>(null);

  // Live run detail — fed to AgentRoster (DagCanvas self-fetches its own copy).
  // Called unconditionally with `undefined` when no run is selected (hooks rule).
  const { detail, refetch } = useRunLive(selectedRunId ?? undefined);

  // ── Run-list rail collapse (titlebar-driven) ────────────────────────────────
  // The left Run-list rail collapse is owned here and driven by the titlebar's
  // session-sider toggle over the shared SESSION_SIDER_TOGGLE_EVENT bus — exactly
  // like ConversationShell owns its session list. We persist under an
  // orchestrator-specific key and broadcast STATE so the titlebar icon stays in
  // sync; when collapsed we simply don't render the rail (mirroring conversation,
  // which hides its session list the same way). Desktop-only: the mobile branch
  // returns a read-only list before any of this rail UI mounts, and the titlebar
  // only exposes the toggle on the desktop orchestrator route.
  const runListSider = useContentSiderCollapse(RUNLIST_COLLAPSE_KEY, false);
  const runListCollapsed = !isMobile && runListSider.collapsed;
  const toggleRunList = runListSider.toggle;

  // Broadcast collapse state so the titlebar toggle reflects it (mount + change).
  useEffect(() => {
    if (isMobile) return;
    dispatchSessionSiderStateEvent(runListSider.collapsed);
  }, [isMobile, runListSider.collapsed]);

  // The titlebar toggle drives collapse via the event bus (desktop only).
  useEffect(() => {
    if (typeof window === 'undefined' || isMobile) return undefined;
    const handler = () => toggleRunList();
    window.addEventListener(SESSION_SIDER_TOGGLE_EVENT, handler);
    return () => window.removeEventListener(SESSION_SIDER_TOGGLE_EVENT, handler);
  }, [isMobile, toggleRunList]);

  // ── Workspace rail availability (titlebar right toggle) ──────────────────────
  // The titlebar workspace button must only appear when a run-workspace rail is
  // actually showing — i.e. a run is open (not composing/re-planning) and its
  // detail carries a work_dir. We broadcast availability from the page (the
  // single source of truth across all detail states), so the empty / compose /
  // no-work_dir states correctly hide the button. RunView owns the collapse
  // itself via useWorkspaceCollapse; this only governs the button's visibility.
  const runWorkspaceAvailable =
    !isMobile && !composing && !replanning && !!selectedRunId && (detail?.run.work_dir?.trim().length ?? 0) > 0;
  useEffect(() => {
    if (isMobile) return undefined;
    dispatchWorkspaceAvailabilityEvent(runWorkspaceAvailable);
    // On leaving the page, reset to unavailable so a stale "available" doesn't
    // linger; the titlebar also resets to true off-route as a backstop.
    return () => dispatchWorkspaceAvailabilityEvent(false);
  }, [isMobile, runWorkspaceAvailable]);

  // Prefill for the re-plan editor: the run's current goal / autonomy, plus its
  // models reconstructed from the fleet snapshot (one ModelRef per member).
  const replanInitial = useMemo<ReplanInitial | undefined>(() => {
    if (!detail) return undefined;
    return {
      goal: detail.run.goal,
      autonomy: detail.run.autonomy === 'supervised' ? 'supervised' : 'interactive',
      models: detail.fleet_members
        .filter((m): m is typeof m & { provider_id: string; model: string } => !!m.provider_id && !!m.model)
        .map((m) => ({ provider_id: m.provider_id, model: m.model })),
    };
  }, [detail]);

  // Selecting a run sets `?run=`; replace:false so browser-back closes it.
  const selectRun = useCallback(
    (id: string) => {
      setComposing(false);
      setReplanning(false);
      setSearchParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          p.set('run', id);
          return p;
        },
        { replace: false }
      );
    },
    [setSearchParams]
  );

  const closeRun = useCallback(() => {
    setSearchParams(
      (prev) => {
        const p = new URLSearchParams(prev);
        p.delete('run');
        return p;
      },
      { replace: false }
    );
  }, [setSearchParams]);

  // 「＋ 新建 Run」lands on the conversational intent box (the quick path): just
  // clear the open run + the composer, so the detail pane falls through to the
  // NewRunIntentBox default surface.
  const startNewRun = useCallback(() => {
    setComposing(false);
    setReplanning(false);
    closeRun();
  }, [closeRun]);

  // Open the full structured composer (advanced control: work_dir / pinned roles
  // / explicit model range / autonomy) — reached from the intent box's
  // 「结构化新建」link, layered on top of the conversational entry.
  const startComposing = useCallback(() => {
    setComposing(true);
    setReplanning(false);
    closeRun();
  }, [closeRun]);

  // A deleted run that's currently open must close (drop `?run=`).
  const handleRunDeleted = useCallback(
    (id: string) => {
      if (selectedRunId === id) closeRun();
    },
    [selectedRunId, closeRun]
  );

  // Closing the run (or leaving compose) dismisses any open transcript drawer
  // and the re-plan editor.
  useEffect(() => {
    if (!selectedRunId) {
      setSelectedTask(null);
      setReplanning(false);
    }
  }, [selectedRunId]);

  // ── Mobile: read-only list / summary (no interactive canvas) ────────────────
  if (isMobile) {
    return (
      <div className='box-border min-h-full w-full overflow-y-auto px-16px py-16px'>
        <div className='text-20px font-600 leading-tight text-t-primary'>{t('orchestrator.title')}</div>
        <div className='mb-14px mt-4px text-12px leading-16px text-t-tertiary'>{t('orchestrator.subtitle')}</div>
        {selectedRunId ? (
          <MobileRunSummary runId={selectedRunId} onBack={closeRun} />
        ) : (
          <RunHistory onOpenRun={selectRun} />
        )}
      </div>
    );
  }

  return (
    <div className='relative flex size-full min-h-0'>
      {/* Run-list rail — hidden when collapsed (titlebar toggle re-expands it),
          mirroring the conversation Tab's session list. */}
      {!runListCollapsed && (
        <RunListRail
          selectedRunId={selectedRunId}
          newRunActive={!selectedRunId}
          onNewRun={startNewRun}
          onSelectRun={selectRun}
          onRunDeleted={handleRunDeleted}
        />
      )}
      {/* Detail pane — three states. */}
      <div className='relative flex min-h-0 min-w-0 flex-1 flex-col' role='tabpanel' aria-label={t('orchestrator.title')}>
        {composing ? (
          <div className='min-h-0 flex-1 overflow-y-auto px-40px py-32px'>
            <NewRunComposer
              onCreated={(runId) => {
                setComposing(false);
                selectRun(runId);
              }}
              onCancel={() => setComposing(false)}
            />
          </div>
        ) : selectedRunId ? (
          replanning && detail ? (
            // In-place re-plan editor — reuses the composer pre-filled with the
            // run's goal / models / autonomy. On submit it clears the old plan
            // and re-decomposes (same run id), then drops back to the run view.
            <div className='min-h-0 flex-1 overflow-y-auto px-40px py-32px'>
              <NewRunComposer
                mode='replan'
                runId={selectedRunId}
                initial={replanInitial}
                onCreated={() => {
                  setReplanning(false);
                  void refetch();
                }}
                onCancel={() => setReplanning(false)}
              />
            </div>
          ) : (
            <RunView
              runId={selectedRunId}
              detail={detail}
              selectedTaskId={selectedTask?.task.id ?? null}
              onSelectTask={setSelectedTask}
              refetch={refetch}
              onBack={closeRun}
              onReplan={() => setReplanning(true)}
            />
          )
        ) : (
          <NewRunIntentBox onCreated={selectRun} onAdvanced={startComposing} />
        )}
      </div>

      {/* Task inspector + worker transcript drawer — always mounted, visible
          when a task node / roster card is clicked. */}
      <WorkerTranscriptPanel open={selectedTask} onClose={() => setSelectedTask(null)} />
    </div>
  );
};

export default OrchestratorPage;
