/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { Suspense, useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { Input, Popconfirm, Spin } from '@arco-design/web-react';
import { Comment, Delete, Edit, ExpandLeft, ExpandRight, Plus, Right, Workbench } from '@icon-park/react';
import { ipcBridge } from '@/common';
import type { TRun } from '@/common/types/orchestrator/orchestratorTypes';
import AppLoader from '@/renderer/components/layout/AppLoader';
import { useLayoutContext } from '@/renderer/hooks/context/LayoutContext';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import RunHistory from './RunHistory';
import NewRunComposer from './NewRunComposer';
import AgentRoster from './RunDetail/AgentRoster';
import WorkerTranscriptPanel from './RunDetail/WorkerTranscriptPanel';
import MobileRunSummary from './RunDetail/MobileRunSummary';
import type { OpenTaskPayload } from './RunDetail/DagCanvas';
import { useMyRuns } from './useOrchestratorData';
import { useRunLive } from './useRunLive';

// The DAG canvas pulls in react-flow (heavy) and is only mounted when a run is
// open, so it is split into its own chunk and loaded on demand.
const DagCanvas = React.lazy(() => import('./RunDetail/DagCanvas'));

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

/** localStorage key for the run-list rail collapse preference. */
const RUNLIST_COLLAPSE_KEY = 'orchestrator-runlist-collapsed';

/**
 * RunListRail — the master column: a prominent 「＋ 新建 Run」button atop a
 * scrollable list of the current user's runs (active + history, newest first via
 * {@link useMyRuns}). Each row carries rename / delete management actions.
 * Selecting a row is the page's primary navigation; the 「新建」button and any
 * open run live in the detail pane on the right. The rail collapses to a slim
 * strip (preference persisted) to give the canvas + right rail more room.
 */
const RunListRail: React.FC<{
  selectedRunId: string | undefined;
  composing: boolean;
  onNewRun: () => void;
  onSelectRun: (id: string) => void;
  onRunDeleted: (id: string) => void;
}> = ({ selectedRunId, composing, onNewRun, onSelectRun, onRunDeleted }) => {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { runs, isLoading, error, mutate } = useMyRuns();
  const [message, msgCtx] = useArcoMessage();

  const [collapsed, setCollapsed] = useState<boolean>(() => {
    try {
      return localStorage.getItem(RUNLIST_COLLAPSE_KEY) === '1';
    } catch {
      return false;
    }
  });
  const toggleCollapsed = useCallback(() => {
    setCollapsed((prev) => {
      const next = !prev;
      try {
        localStorage.setItem(RUNLIST_COLLAPSE_KEY, next ? '1' : '0');
      } catch {
        /* ignore persistence failures */
      }
      return next;
    });
  }, []);

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

  // Collapsed: a slim strip with just the new-run + expand affordances.
  if (collapsed) {
    return (
      <div className='flex h-full w-48px shrink-0 flex-col items-center gap-8px border-r border-r-base bg-1 py-12px'>
        {msgCtx}
        <RailIconButton label={t('orchestrator.tab.expand')} onClick={toggleCollapsed}>
          <ExpandRight theme='outline' size='16' strokeWidth={3} />
        </RailIconButton>
        <RailIconButton label={t('orchestrator.tab.newRun')} primary onClick={onNewRun}>
          <Plus theme='outline' size='16' strokeWidth={4} />
        </RailIconButton>
      </div>
    );
  }

  return (
    <div className='flex h-full min-h-0 w-300px shrink-0 flex-col border-r border-r-base bg-1'>
      {msgCtx}
      {/* Header + new-run button */}
      <div className='shrink-0 px-16px pt-16px pb-12px'>
        <div className='flex items-start gap-8px'>
          <div className='min-w-0 flex-1'>
            <div className='text-15px font-600 leading-tight text-t-primary'>{t('orchestrator.title')}</div>
            <div className='mt-2px text-11px leading-15px text-t-tertiary'>{t('orchestrator.subtitle')}</div>
          </div>
          <RailIconButton label={t('orchestrator.tab.collapse')} onClick={toggleCollapsed}>
            <ExpandLeft theme='outline' size='15' strokeWidth={3} />
          </RailIconButton>
        </div>
        <div
          role='button'
          tabIndex={0}
          aria-pressed={composing}
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
            boxShadow: composing ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 22%, transparent)' : undefined,
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
                selected={!composing && selectedRunId === run.id}
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

/** A square icon button used in the rail header / collapsed strip. */
const RailIconButton: React.FC<{
  label: string;
  primary?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}> = ({ label, primary, onClick, children }) => (
  <div
    role='button'
    tabIndex={0}
    aria-label={label}
    title={label}
    onClick={onClick}
    onKeyDown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onClick();
      }
    }}
    className={
      primary
        ? 'flex size-30px shrink-0 cursor-pointer items-center justify-center rd-8px text-white transition-opacity hover:opacity-90'
        : 'flex size-26px shrink-0 cursor-pointer items-center justify-center rd-7px text-t-tertiary transition-colors hover:bg-fill-2 hover:text-t-primary'
    }
    style={primary ? { background: 'rgb(var(--primary-6))' } : undefined}
  >
    {children}
  </div>
);

/** The clean empty state shown when nothing is selected and not composing. */
const EmptyDetail: React.FC = () => {
  const { t } = useTranslation();
  return (
    <div className='flex size-full min-h-0 flex-col items-center justify-center gap-14px px-24px text-center'>
      <span className='flex size-56px items-center justify-center rd-16px bg-fill-2 text-t-tertiary'>
        <Workbench theme='outline' size='28' strokeWidth={3} />
      </span>
      <div className='text-16px font-600 text-t-primary'>{t('orchestrator.empty.title')}</div>
      <div className='max-w-360px text-12px leading-18px text-t-tertiary'>{t('orchestrator.empty.desc')}</div>
    </div>
  );
};

/**
 * OrchestratorPage (/orchestrator) — 「智能编排」(orchestration), rebuilt as a
 * master-detail workspace. The left rail ({@link RunListRail}) lists the user's
 * runs with a prominent 「＋ 新建 Run」button; the detail pane has three states:
 *
 *  1. **composing** — {@link NewRunComposer} (after pressing 「＋ 新建 Run」).
 *     On `onCreated` we select the new run; on `onCancel` we drop back.
 *  2. **a run selected** (`?run=<id>`) — the run view: an {@link AgentRoster}
 *     strip atop the interactive {@link DagCanvas} (which itself renders the
 *     run-detail header + status-aware controls + the completed-run role
 *     precipitation panel, and wires cancel/approve/pause/resume internally —
 *     so we don't duplicate those here). Clicking a roster card or a DAG node
 *     opens the {@link WorkerTranscriptPanel} drawer.
 *  3. **nothing selected** — a clean {@link EmptyDetail} prompt.
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
  // The clicked DAG node / roster card payload → opens the transcript drawer.
  const [selectedTask, setSelectedTask] = useState<OpenTaskPayload | null>(null);

  // Live run detail — fed to AgentRoster (DagCanvas self-fetches its own copy).
  // Called unconditionally with `undefined` when no run is selected (hooks rule).
  const { detail, refetch } = useRunLive(selectedRunId ?? undefined);

  // Selecting a run sets `?run=`; replace:false so browser-back closes it.
  const selectRun = useCallback(
    (id: string) => {
      setComposing(false);
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

  const startComposing = useCallback(() => {
    setComposing(true);
    closeRun();
  }, [closeRun]);

  // A deleted run that's currently open must close (drop `?run=`).
  const handleRunDeleted = useCallback(
    (id: string) => {
      if (selectedRunId === id) closeRun();
    },
    [selectedRunId, closeRun]
  );

  // Closing the run (or leaving compose) dismisses any open transcript drawer.
  useEffect(() => {
    if (!selectedRunId) setSelectedTask(null);
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
      <RunListRail
        selectedRunId={selectedRunId}
        composing={composing}
        onNewRun={startComposing}
        onSelectRun={selectRun}
        onRunDeleted={handleRunDeleted}
      />

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
          <>
            {/* AgentRoster sits above the canvas; DagCanvas brings its own header,
                run controls, and completed-run precipitation panel. */}
            {detail && (
              <AgentRoster
                detail={detail}
                selectedTaskId={selectedTask?.task.id ?? null}
                onSelectTask={setSelectedTask}
                refetch={refetch}
              />
            )}
            <div className='min-h-0 flex-1 overflow-hidden'>
              <Suspense fallback={<AppLoader />}>
                <DagCanvas runId={selectedRunId} onBack={closeRun} onOpenTask={setSelectedTask} />
              </Suspense>
            </div>
          </>
        ) : (
          <EmptyDetail />
        )}
      </div>

      {/* Task inspector + worker transcript drawer — always mounted, visible
          when a task node / roster card is clicked. */}
      <WorkerTranscriptPanel open={selectedTask} onClose={() => setSelectedTask(null)} />
    </div>
  );
};

export default OrchestratorPage;
