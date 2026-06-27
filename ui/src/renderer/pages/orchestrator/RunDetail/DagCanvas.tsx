/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ReactFlow, Background, BackgroundVariant, Controls, MiniMap, type Edge, type ReactFlowInstance } from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import './dag-canvas.css';
import { Branch } from '@icon-park/react';
import { Spin } from '@arco-design/web-react';
import { ipcBridge } from '@/common';
import type { TAssignment, TFleetMember, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import { useRunLive } from '../useRunLive';
import { layoutDag } from './layoutDag';
import { memberLogo, memberShortLabel } from './memberLabel';
import RolePrecipitationPanel from './RolePrecipitationPanel';
import RunDetailHeader from './RunDetailHeader';
import TaskNode, { taskStatusMeta, type TaskFlowNode } from './nodes/TaskNode';

/** Stable nodeTypes ref so react-flow doesn't warn about a new object each render. */
const NODE_TYPES = { task: TaskNode } as const;

/** fitView tuning — shared by the static `fitView` prop (initial mount) and the
 * ResizeObserver-driven refit (see below). A small padding keeps the DAG from
 * wasting the narrow conversation rail's width, while a generous maxZoom lets a
 * small (1-2 node) graph grow to a legible size instead of staying pinned tiny. */
const FIT_VIEW_OPTIONS = { padding: 0.12, maxZoom: 1.6 } as const;

/** Statuses that count as "done" for the aggregate progress pill. */
const DONE_STATUSES = new Set(['done', 'completed', 'skipped', 'cancelled']);

/** Payload handed up when a DAG node is clicked — everything the task inspector
 * needs to show the assignment rationale and offer reassign/lock, without the
 * panel having to re-fetch the run. `refetch` re-pulls the run detail so the
 * canvas + inspector reflect a reassignment immediately. */
export interface OpenTaskPayload {
  task: TRunTask;
  assignment: TAssignment | null;
  fleetMembers: TFleetMember[];
  runId: string;
  refetch: () => Promise<void>;
}

interface DagCanvasProps {
  runId: string;
  onBack: () => void;
  onOpenTask: (payload: OpenTaskPayload) => void;
  /**
   * Embedded mode — the canvas lives inside a conversation's workspace rail tab
   * (no master-detail to return to), so the header's back button is suppressed
   * while the run controls (cancel/approve/pause/resume) are kept. The standalone
   * orchestrator page omits this prop, so its back button still renders.
   */
  embedded?: boolean;
}

/**
 * DagCanvas — the visual centerpiece of 「智能编排」. Renders a run's task DAG as
 * an interactive react-flow graph: each task is a custom {@link TaskNode}, each
 * `blocker → blocked` dependency is an edge (animated while the downstream task
 * runs). Live-updates via {@link useRunLive}; clicking a node opens the worker
 * transcript panel (Task 5) through `onOpenTask`.
 *
 * Positions prefer the task's persisted `graph_x/graph_y` and otherwise fall
 * back to a topological auto-layout ({@link layoutDag}). react-flow's JS-side
 * colors (MiniMap mask, Background dots) can't read CSS vars, so we mirror the
 * `data-theme` attribute into `colorMode` + resolved colors via a MutationObserver
 * (template: MermaidBlock).
 */
const DagCanvas: React.FC<DagCanvasProps> = ({ runId, onBack, onOpenTask, embedded }) => {
  const { t } = useTranslation();
  const { detail, loading, refetch } = useRunLive(runId);
  const [message, ctx] = useArcoMessage();
  const [busy, setBusy] = useState(false);

  // The static `fitView` prop fits ONCE at initial mount. In the conversation
  // rail (DagRailTab) the canvas mounts while the rail is COLLAPSED (≈0-size
  // container), so that initial fit happens against a zero viewport and leaves
  // the nodes tiny in a corner; when the rail later auto-expands, react-flow
  // never re-fits on its own. We capture the instance via `onInit` and re-run
  // `fitView()` whenever the wrapper transitions from ~0 → a real size (i.e.
  // becomes visible). The standalone orchestrator page is sized at mount, so it
  // simply gets a harmless extra refit. `wasVisibleRef` guards against thrashing
  // by only firing on the 0→visible edge.
  const rfRef = useRef<ReactFlowInstance<TaskFlowNode, Edge> | null>(null);
  const flowWrapRef = useRef<HTMLDivElement | null>(null);
  const wasVisibleRef = useRef(false);
  useEffect(() => {
    const el = flowWrapRef.current;
    if (!el) return;
    let raf = 0;
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (!entry) return;
      const { width, height } = entry.contentRect;
      const visible = width > 0 && height > 0;
      // Only refit on the collapsed→visible edge so dragging the rail wider
      // (already visible) doesn't yank the viewport out from under the user.
      if (visible && !wasVisibleRef.current) {
        cancelAnimationFrame(raf);
        // Defer one frame so react-flow has measured the new viewport before we fit.
        raf = requestAnimationFrame(() => {
          rfRef.current?.fitView(FIT_VIEW_OPTIONS);
        });
      }
      wasVisibleRef.current = visible;
    });
    observer.observe(el);
    return () => {
      cancelAnimationFrame(raf);
      observer.disconnect();
    };
  }, []);

  // Mirror the global data-theme attribute (light/dark) for react-flow internals
  // whose colors are JS props (MiniMap mask, Background dots) and cannot read CSS
  // vars. Same observer pattern as MermaidBlock.
  const [theme, setTheme] = useState<'light' | 'dark'>(() =>
    (document.documentElement.getAttribute('data-theme') as 'light' | 'dark') || 'light'
  );
  useEffect(() => {
    const update = () => {
      setTheme((document.documentElement.getAttribute('data-theme') as 'light' | 'dark') || 'light');
    };
    const observer = new MutationObserver(update);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    return () => observer.disconnect();
  }, []);

  // Resolved JS-side colors for react-flow internals (theme-matched, no CSS vars).
  const flowColors = useMemo(
    () =>
      theme === 'dark'
        ? { dots: '#333333', minimapMask: 'rgba(0,0,0,0.55)', minimapBg: '#1a1a1a', minimapStroke: '#404040' }
        : { dots: '#d1d5e5', minimapMask: 'rgba(255,255,255,0.6)', minimapBg: '#f9fafb', minimapStroke: '#e5e6eb' },
    [theme]
  );

  // task_id → assignment (for the node chip + the inspector).
  const assignmentByTask = useMemo(() => {
    const map = new Map<string, TAssignment>();
    for (const a of detail?.assignments ?? []) map.set(a.task_id, a);
    return map;
  }, [detail?.assignments]);

  // member_id → fleet member (the run's fleet snapshot) for friendly labels.
  const memberById = useMemo(() => {
    const map = new Map<string, TFleetMember>();
    for (const m of detail?.fleet_members ?? []) map.set(m.id, m);
    return map;
  }, [detail?.fleet_members]);

  const fleetMembers = useMemo(() => detail?.fleet_members ?? [], [detail?.fleet_members]);

  const handleOpenTask = useCallback(
    (task: TRunTask) => {
      onOpenTask({
        task,
        assignment: assignmentByTask.get(task.id) ?? null,
        fleetMembers,
        runId,
        refetch,
      });
    },
    [onOpenTask, assignmentByTask, fleetMembers, runId, refetch]
  );

  const nodes = useMemo<TaskFlowNode[]>(() => {
    const tasks = detail?.tasks ?? [];
    const deps = detail?.deps ?? [];
    if (tasks.length === 0) return [];
    const fallback = layoutDag(tasks, deps);
    return tasks.map((task) => {
      const pos =
        task.graph_x != null && task.graph_y != null
          ? { x: task.graph_x, y: task.graph_y }
          : (fallback[task.id] ?? { x: 0, y: 0 });
      const assignment = assignmentByTask.get(task.id);
      const member = assignment ? memberById.get(assignment.member_id) : undefined;
      // Friendly label from the fleet snapshot; fall back to the localized
      // "assigned" pill if the member can't be resolved (still better than a uuid).
      const friendly = memberShortLabel(member);
      return {
        id: task.id,
        type: 'task',
        position: pos,
        data: {
          title: task.title || t('orchestrator.run.detail.untitledTask'),
          status: task.status,
          statusLabel: t(`orchestrator.run.task.status.${task.status}`, {
            defaultValue: t('orchestrator.run.status.unknown'),
          }),
          memberId: assignment?.member_id,
          chipLabel: assignment ? (friendly ?? t('orchestrator.run.detail.assigned')) : undefined,
          memberLogo: memberLogo(member),
          locked: assignment?.locked ?? false,
          attempt: task.attempt,
          onOpen: () => handleOpenTask(task),
        },
      };
    });
  }, [detail?.tasks, detail?.deps, assignmentByTask, memberById, handleOpenTask, t]);

  const edges = useMemo<Edge[]>(() => {
    const tasks = detail?.tasks ?? [];
    const deps = detail?.deps ?? [];
    const statusById = new Map(tasks.map((task) => [task.id, task.status]));
    return deps.map((dep) => {
      const downstreamRunning = statusById.get(dep.blocked_task_id) === 'running';
      return {
        id: `${dep.blocker_task_id}->${dep.blocked_task_id}`,
        source: dep.blocker_task_id,
        target: dep.blocked_task_id,
        animated: downstreamRunning,
        style: {
          stroke: downstreamRunning ? 'rgb(var(--primary-6))' : 'var(--border-base)',
          strokeWidth: downstreamRunning ? 2 : 1.5,
        },
      };
    });
  }, [detail?.tasks, detail?.deps]);

  const { done, total } = useMemo(() => {
    const tasks = detail?.tasks ?? [];
    return {
      done: tasks.filter((task) => DONE_STATUSES.has(task.status)).length,
      total: tasks.length,
    };
  }, [detail?.tasks]);

  const handleCancel = async () => {
    setBusy(true);
    try {
      await ipcBridge.orchestrator.runs.cancel.invoke({ id: runId });
      message.success(t('orchestrator.run.detail.cancelOk'));
      await refetch();
    } catch (e) {
      message.error(t('orchestrator.run.detail.cancelError', { error: String(e) }));
    } finally {
      setBusy(false);
    }
  };

  const handleApprove = async () => {
    setBusy(true);
    try {
      await ipcBridge.orchestrator.runs.approve.invoke({ id: runId });
      message.success(t('orchestrator.run.detail.approveOk'));
      await refetch();
    } catch (e) {
      message.error(t('orchestrator.run.detail.approveError', { error: String(e) }));
    } finally {
      setBusy(false);
    }
  };

  const handlePause = async () => {
    setBusy(true);
    try {
      await ipcBridge.orchestrator.runs.pause.invoke({ id: runId });
      message.success(t('orchestrator.run.detail.pauseOk'));
      await refetch();
    } catch (e) {
      message.error(t('orchestrator.run.detail.pauseError', { error: String(e) }));
    } finally {
      setBusy(false);
    }
  };

  const handleResume = async () => {
    setBusy(true);
    try {
      await ipcBridge.orchestrator.runs.resume.invoke({ id: runId });
      message.success(t('orchestrator.run.detail.resumeOk'));
      await refetch();
    } catch (e) {
      message.error(t('orchestrator.run.detail.resumeError', { error: String(e) }));
    } finally {
      setBusy(false);
    }
  };

  // First load with no detail yet.
  if (loading && !detail) {
    return (
      <div className='flex size-full min-h-0 flex-col'>
        <div className='flex flex-1 items-center justify-center'>
          <Spin />
        </div>
      </div>
    );
  }

  if (!detail) {
    return (
      <div className='flex size-full min-h-0 flex-col items-center justify-center gap-12px px-24px text-center'>
        <span className='flex size-48px items-center justify-center rd-14px bg-fill-2 text-t-tertiary'>
          <Branch theme='outline' size='24' strokeWidth={3} />
        </span>
        <div className='text-15px font-600 text-t-primary'>{t('orchestrator.run.detail.loadError')}</div>
      </div>
    );
  }

  const noTasks = detail.tasks.length === 0;

  return (
    <div className='size-full min-h-0 flex flex-col'>
      {ctx}
      <RunDetailHeader
        run={detail.run}
        done={done}
        total={total}
        embedded={embedded}
        onBack={onBack}
        onCancel={() => void handleCancel()}
        onApprove={() => void handleApprove()}
        onPause={() => void handlePause()}
        onResume={() => void handleResume()}
        busy={busy}
      />

      {/* Role precipitation — when the run is done, suggest saving its used
          roles as assistants. Lives as a `shrink-0` sibling above the canvas so
          the react-flow region keeps its `flex-1 min-h-0` sizing intact. The
          panel renders nothing when there are no roles / all already exist. */}
      {detail.run.status === 'completed' && <RolePrecipitationPanel detail={detail} />}

      <div ref={flowWrapRef} className='flex-1 min-h-0'>
        {noTasks ? (
          <div className='flex size-full flex-col items-center justify-center gap-12px px-24px text-center'>
            <span className='nomi-dag-pulse flex size-52px items-center justify-center rd-16px bg-fill-2 text-primary-6'>
              <Branch theme='outline' size='26' strokeWidth={3} />
            </span>
            <div className='text-15px font-600 text-t-primary'>{t('orchestrator.run.detail.planningTitle')}</div>
            <div className='max-w-320px text-12px leading-18px text-t-tertiary'>
              {t('orchestrator.run.detail.planningDesc')}
            </div>
          </div>
        ) : (
          <ReactFlow
            className='nomi-dag-flow'
            onInit={(instance) => {
              rfRef.current = instance;
            }}
            nodes={nodes}
            edges={edges}
            nodeTypes={NODE_TYPES}
            colorMode={theme}
            fitView
            fitViewOptions={FIT_VIEW_OPTIONS}
            minZoom={0.2}
            maxZoom={1.8}
            proOptions={{ hideAttribution: true }}
            nodesConnectable={false}
            nodesDraggable
            elementsSelectable
          >
            <Background variant={BackgroundVariant.Dots} gap={20} size={1.4} color={flowColors.dots} />
            <Controls showInteractive={false} />
            <MiniMap
              pannable
              zoomable
              maskColor={flowColors.minimapMask}
              style={{ background: flowColors.minimapBg, border: `1px solid ${flowColors.minimapStroke}` }}
              nodeColor={(n) => taskStatusMeta(String((n.data as { status?: string }).status ?? '')).color}
              nodeStrokeWidth={2}
            />
          </ReactFlow>
        )}
      </div>
    </div>
  );
};

export default DagCanvas;
