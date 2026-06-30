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
import type { TAssignment, TFleetMember, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import { useRunLive } from '../useRunLive';
import { layoutDag } from './layoutDag';
import { memberLogo, memberShortLabel } from './memberLabel';
import RolePrecipitationPanel from './RolePrecipitationPanel';
import TaskNode, { normalizeTaskKind, type TaskFlowNode, type JudgeWinner, type LoopState, type VerifyVerdict } from './nodes/TaskNode';
import MainNode, { type MainFlowNode } from './nodes/MainNode';

/** Stable nodeTypes refs so react-flow doesn't warn about a new object each
 * render. Two frozen variants keep the backward-compatible path byte-identical:
 * `NODE_TYPES` (task-only) is used when `onOpenMain` is absent — exactly as
 * before — and `NODE_TYPES_WITH_MAIN` additionally registers the synthetic main
 * node only when a consumer opts into it. */
const NODE_TYPES = { task: TaskNode } as const;
const NODE_TYPES_WITH_MAIN = { task: TaskNode, main: MainNode } as const;

/** Stable id for the synthetic lead/main agent node injected above the root
 * tasks when `onOpenMain` is provided. Underscore-fenced so it can never collide
 * with a real task id. */
const MAIN_NODE_ID = '__main__';

/** Vertical offset (px) applied to the OUTPUT of {@link layoutDag} so every task
 * node shifts down one row, freeing the top row for the synthetic main node.
 * Applied ONLY when the main node is injected; `layoutDag` itself is never
 * touched. Matches `layoutDag`'s own `ROW_STEP` so the main→root spacing reads
 * as one consistent dependency layer. */
const MAIN_ROW_OFFSET = 140;

/** Any node this canvas can render — a task node, or (only when `onOpenMain` is
 * given) the synthetic main node. */
type DagFlowNode = TaskFlowNode | MainFlowNode;

/** Neutral verdict for a verify node whose marker is absent / unparseable / still
 * settling — renders the pill in its neutral "verifying…" state instead of a
 * pass/fail tone. */
const NEUTRAL_VERDICT: VerifyVerdict = { pass: null, tally: null };

/** Leading `VERDICT:` marker a verify task writes to its `output_summary`
 * (`render_verify_summary` in engine.rs), e.g.
 *   `VERDICT: PASS (2/3 skeptics passed, policy=majority)`
 *   `VERDICT: FAIL (1/2 skeptics passed, policy=...)`
 * We only need the pass/fail word and the `m/n` tally; the rest is free text. */
const VERDICT_RE = /^VERDICT:\s+(PASS|FAIL)\s+\((\d+)\/(\d+)/;

/**
 * Parse a verify task's `output_summary` into a {@link VerifyVerdict}. Defensive
 * by design: a missing/empty/unparseable summary (still verifying, legacy data,
 * or a malformed marker) yields the neutral `{ pass: null, tally: null }` so the
 * pill shows "verifying…" instead of ever throwing on the canvas.
 */
function parseVerifyVerdict(outputSummary: string | null | undefined): VerifyVerdict {
  if (!outputSummary) return NEUTRAL_VERDICT;
  const m = VERDICT_RE.exec(outputSummary.trim());
  if (!m) return NEUTRAL_VERDICT;
  return { pass: m[1] === 'PASS', tally: `${m[2]}/${m[3]}` };
}

/** Neutral winner for a judge node whose marker is absent / `none` / unparseable
 * / still settling — renders the pill in its neutral "no winner / judging…"
 * state instead of a success tone. */
const NEUTRAL_WINNER: JudgeWinner = { winner: null, aggregate: null, judges: null };

/** Leading `WINNER:` marker a judge aggregator writes to its `output_summary`
 * (`render_judge_summary` in engine.rs), e.g.
 *   `WINNER: candidate 0 (aggregate=mean, scores=[…], judges=2/3)`
 *   `WINNER: none (aggregate=borda, scores=[…], judges=0/2)`
 * We pull the 0-based candidate index (or `none`); the `aggregate` policy and the
 * `judges=c/n` tally are captured optionally so the pill can surface them. */
const WINNER_RE = /^WINNER:\s+(?:candidate\s+(\d+)|none)/;
const WINNER_AGG_RE = /aggregate=(mean|borda)/;
const WINNER_JUDGES_RE = /judges=(\d+\/\d+)/;

/**
 * Parse a judge task's `output_summary` into a {@link JudgeWinner}. Defensive by
 * design: a missing/empty/unparseable summary, or a `none` marker (no candidates
 * / no usable ballots / still judging), yields `winner: null` so the pill shows
 * the neutral "no winner / judging…" state instead of ever throwing on the canvas.
 * The `aggregate` policy and `judges=c/n` tally are best-effort extras (null when
 * absent) and never gate the winner parse.
 */
function parseJudgeWinner(outputSummary: string | null | undefined): JudgeWinner {
  if (!outputSummary) return NEUTRAL_WINNER;
  const trimmed = outputSummary.trim();
  const m = WINNER_RE.exec(trimmed);
  if (!m) return NEUTRAL_WINNER;
  // m[1] is the candidate index for `candidate K`; undefined for the `none` arm.
  const winner = m[1] != null ? Number.parseInt(m[1], 10) : null;
  const aggMatch = WINNER_AGG_RE.exec(trimmed);
  const aggregate = aggMatch ? (aggMatch[1] as 'mean' | 'borda') : null;
  const judgesMatch = WINNER_JUDGES_RE.exec(trimmed);
  return {
    winner: winner != null && Number.isFinite(winner) ? winner : null,
    aggregate,
    judges: judgesMatch ? judgesMatch[1] : null,
  };
}

/** Neutral state for a loop controller whose marker is absent / a transient
 * `LOOP-STATE:` line / unparseable (still iterating) — renders the pill in its
 * neutral "iterating…" state instead of a done/failed tone. */
const NEUTRAL_LOOP: LoopState = { state: null, reason: null, iterations: null, maxIter: null };

/** Leading `LOOP:` marker a loop CONTROLLER writes to its `output_summary` on
 * stop (`render_loop_final` in engine.rs), e.g.
 *   `LOOP: DONE (reason=max_iter, iterations=3, max_iter=3)`
 *   `LOOP: FAILED (reason=body_failed, iterations=1, max_iter=5)`
 * While still iterating, the controller stays `pending` and its summary holds a
 * transient `LOOP-STATE: hashes=…` line (or is empty) — any non-`LOOP:` lead is
 * treated as the neutral "iterating…" state. We pull the stop word + reason +
 * the iterations/max_iter counts. */
const LOOP_RE = /^LOOP:\s+(DONE|FAILED)\s+\(reason=([a-z_]+),\s*iterations=(\d+),\s*max_iter=(\d+)\)/;

/**
 * Parse a loop controller's `output_summary` into a {@link LoopState}. Defensive
 * by design: a missing/empty summary, a transient `LOOP-STATE:` line (still
 * iterating), or a malformed marker yields the neutral
 * `{ state: null, reason: null, iterations: null, maxIter: null }` so the pill
 * shows "iterating…" instead of ever throwing on the canvas.
 */
function parseLoopState(outputSummary: string | null | undefined): LoopState {
  if (!outputSummary) return NEUTRAL_LOOP;
  const m = LOOP_RE.exec(outputSummary.trim());
  if (!m) return NEUTRAL_LOOP;
  const iterations = Number.parseInt(m[3], 10);
  const maxIter = Number.parseInt(m[4], 10);
  return {
    state: m[1] === 'DONE' ? 'done' : 'failed',
    reason: m[2],
    iterations: Number.isFinite(iterations) ? iterations : null,
    maxIter: Number.isFinite(maxIter) ? maxIter : null,
  };
}

/**
 * Defensively pull the fan-out group label out of a task's `pattern_config`
 * (a raw JSON string, e.g. `{"group":"research"}`). Returns the trimmed label
 * or `undefined` for anything malformed — null/empty, non-JSON, non-object, or
 * a missing/blank `group` — so a bad payload never throws on the canvas.
 */
function parseGroupLabel(patternConfig: string | null | undefined): string | undefined {
  if (!patternConfig) return undefined;
  try {
    const parsed: unknown = JSON.parse(patternConfig);
    if (parsed && typeof parsed === 'object' && 'group' in parsed) {
      const group = (parsed as { group: unknown }).group;
      if (typeof group === 'string') {
        const trimmed = group.trim();
        return trimmed.length > 0 ? trimmed : undefined;
      }
    }
  } catch {
    // Malformed JSON → no group (no crash).
  }
  return undefined;
}

/** Deterministic hue (0–359°) from a group label so every sibling in a fan-out
 * group shares one calm tint, stable across re-renders and live refetches. */
function hueForGroup(label: string): number {
  let hash = 0;
  for (let i = 0; i < label.length; i += 1) {
    hash = (hash * 31 + label.charCodeAt(i)) % 360;
  }
  return hash;
}

/**
 * The set of in-degree-0 ("root") task ids — tasks with no blocker among the
 * run's real tasks. Mirrors {@link layoutDag}'s edge filter (only deps whose
 * BOTH endpoints are real tasks count) so the roots we connect the synthetic
 * main node to are exactly the tasks `layoutDag` places at its top layer.
 * Used ONLY on the `onOpenMain`-provided path; never affects the default path.
 */
function computeRootTaskIds(
  tasks: TRunTask[],
  deps: { blocker_task_id: string; blocked_task_id: string }[]
): Set<string> {
  const taskIds = new Set(tasks.map((task) => task.id));
  const hasBlocker = new Set<string>();
  for (const dep of deps) {
    if (taskIds.has(dep.blocker_task_id) && taskIds.has(dep.blocked_task_id)) {
      hasBlocker.add(dep.blocked_task_id);
    }
  }
  const roots = new Set<string>();
  for (const id of taskIds) {
    if (!hasBlocker.has(id)) roots.add(id);
  }
  return roots;
}

/** fitView tuning — shared by the static `fitView` prop (initial mount) and the
 * ResizeObserver-driven refit (see below). A small padding keeps the DAG from
 * wasting the narrow conversation rail's width, while a generous maxZoom lets a
 * small (1-2 node) graph grow to a legible size instead of staying pinned tiny. */
const FIT_VIEW_OPTIONS = { padding: 0.12, maxZoom: 1.6 } as const;

/**
 * Size hints stamped onto every node object as `initialWidth`/`initialHeight`.
 *
 * WHY this exists — the MiniMap was rendering BLANK. react-flow's MiniMapNode
 * (`@xyflow/react` v12) reads each node's size off the *user* node object via
 * `getNodeDimensions` (= `measured?.w ?? width ?? initialWidth ?? 0`) and, when
 * that resolves to 0, `nodeHasDimensions` is false and the wrapper returns
 * `null` — so no `<rect>` is emitted at all. Our nodes get their size purely
 * from the `w-220px` UnoCSS class on the rendered card; the measured dimensions
 * are written to the *internal* node, never back onto the user node objects the
 * minimap reads (this canvas drives `nodes` as controlled props with no
 * `onNodesChange` write-back). Result: the minimap sees 0×0 forever → blank.
 *
 * `initialWidth`/`initialHeight` (NOT `width`/`height`) are the right knob: they
 * give `getNodeDimensions`/`nodeHasDimensions` a non-zero size so the minimap
 * rect renders, but `getNodeInlineStyleDimensions` only applies `initialWidth/
 * Height` to the rendered DOM *before first measurement* and then defers to the
 * CSS-driven natural size — so the main canvas card keeps its `w-220px` +
 * auto-height (no clipping, no fixed-height distortion of the meta-chip rows).
 * The width mirrors the card (`w-220px`); the height is a representative card
 * height — its only effect is the minimap rect's aspect ratio. */
const MINIMAP_NODE_W = 220;
const MINIMAP_NODE_H = 96;

/**
 * The MiniMap's LITERAL mirror of {@link taskStatusMeta}.
 *
 * The minimap's `nodeColor` becomes the SVG `<rect>`'s `fill`, and react-flow
 * resolves it as a plain JS prop — CSS-var expressions (`var(--success)`,
 * `rgb(var(--primary-6))`, …) are fragile / unresolved in that context, exactly
 * like the mask/bg/stroke we already hex-mirror into `flowColors`. Worse, two of
 * the status colors are near-background light greys — `pending → var(--bg-6)`
 * and `skipped/cancelled → var(--text-disabled)` — which VANISH against the
 * light minimap bg (`#f9fafb`), so even a perfectly-sized rect would look blank
 * on a fresh all-pending run.
 *
 * So we resolve every status to a theme-aware LITERAL that contrasts with the
 * minimap bg on BOTH themes: running = brand blue, done = green, failed = red,
 * needs_review/blocked = amber, skipped/cancelled = a muted-but-VISIBLE grey,
 * pending = a visible grey (never near-white). Keep this set in lockstep with
 * `taskStatusMeta`'s switch. */
const MINIMAP_STATUS_COLORS: Record<'light' | 'dark', Record<string, string>> = {
  light: {
    running: '#2f6bff', // brand blue
    done: '#16a34a',
    completed: '#16a34a',
    failed: '#dc2626',
    error: '#dc2626',
    needs_review: '#d97706', // amber
    blocked: '#d97706',
    skipped: '#94a3b8', // muted but visible grey on #f9fafb
    cancelled: '#94a3b8',
    pending: '#b4bccb', // visible grey (not near-white)
  },
  dark: {
    running: '#5b8bff',
    done: '#22c55e',
    completed: '#22c55e',
    failed: '#f04438',
    error: '#f04438',
    needs_review: '#f59e0b',
    blocked: '#f59e0b',
    skipped: '#64748b', // muted but visible grey on #1a1a1a
    cancelled: '#64748b',
    pending: '#5a6273', // visible grey (not near-black)
  },
};

/** Resolve a task status to its minimap-literal color for the active theme,
 * falling back to the `pending` grey for any unknown status (mirrors
 * `taskStatusMeta`'s default arm). */
function miniMapNodeColor(status: string, theme: 'light' | 'dark'): string {
  const map = MINIMAP_STATUS_COLORS[theme];
  return map[status] ?? map.pending;
}

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
  onOpenTask: (payload: OpenTaskPayload) => void;
  /** When provided, render a synthetic lead/main agent node above the root tasks
   * (wired to this callback). When ABSENT, the canvas behaves EXACTLY as before
   * — no main node, no extra edges, no layout shift (backward compatibility for
   * the still-living RunView consumer until F9). */
  onOpenMain?: () => void;
  /** When the main node is rendered, highlight it (the lead conversation is the
   * currently-projected view). Ignored when `onOpenMain` is absent. */
  mainActive?: boolean;
}

/**
 * DagCanvas — the visual centerpiece of 「智能编排」. Renders a run's task DAG as
 * an interactive react-flow graph: each task is a custom {@link TaskNode}, each
 * `blocker → blocked` dependency is an edge (animated while the downstream task
 * runs). Live-updates via {@link useRunLive}; clicking a node opens the worker
 * transcript panel (Task 5) through `onOpenTask`.
 *
 * The run title + status + run controls (cancel / approve / pause / resume) used
 * to live in this canvas' own header; they were lifted UP into {@link RunView}'s
 * shared conversation-style glass header (Task F3) so they are reachable from
 * both the 对话 and 编排画布 views and rendered exactly once. This component is
 * now canvas-only: the react-flow graph, the planning / empty states, and the
 * completed-run {@link RolePrecipitationPanel}.
 *
 * Positions prefer the task's persisted `graph_x/graph_y` and otherwise fall
 * back to a topological auto-layout ({@link layoutDag}). react-flow's JS-side
 * colors (MiniMap mask, Background dots) can't read CSS vars, so we mirror the
 * `data-theme` attribute into `colorMode` + resolved colors via a MutationObserver
 * (template: MermaidBlock).
 */
const DagCanvas: React.FC<DagCanvasProps> = ({ runId, onOpenTask, onOpenMain, mainActive }) => {
  const { t } = useTranslation();
  const { detail, loading, refetch } = useRunLive(runId);

  // The static `fitView` prop fits ONCE at initial mount. If the canvas ever
  // mounts inside a COLLAPSED / ~0-size
  // container, that initial fit happens against a zero viewport and leaves
  // the nodes tiny in a corner; when the container later expands, react-flow
  // never re-fits on its own. We capture the instance via `onInit` and re-run
  // `fitView()` whenever the wrapper transitions from ~0 → a real size (i.e.
  // becomes visible). The standalone orchestrator page is sized at mount, so it
  // simply gets a harmless extra refit. `wasVisibleRef` guards against thrashing
  // by only firing on the 0→visible edge.
  const rfRef = useRef<ReactFlowInstance<DagFlowNode, Edge> | null>(null);
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

  const nodes = useMemo<DagFlowNode[]>(() => {
    const tasks = detail?.tasks ?? [];
    const deps = detail?.deps ?? [];
    if (tasks.length === 0) return [];
    const fallback = layoutDag(tasks, deps);
    // Resolve each fan-out group's shared hue once (deterministic from the label)
    // so every sibling in a group lands on the same tint.
    const hueByGroup = new Map<string, number>();
    for (const task of tasks) {
      const group = parseGroupLabel(task.pattern_config);
      if (group && !hueByGroup.has(group)) hueByGroup.set(group, hueForGroup(group));
    }
    // BACKWARD COMPAT: when no `onOpenMain` is given we shift NOTHING — `offsetY`
    // is 0 so the position object is computed exactly as before (and the array
    // below holds only task nodes, with no injected main / edges).
    const withMain = onOpenMain != null;
    const offsetY = withMain ? MAIN_ROW_OFFSET : 0;
    const taskNodes: TaskFlowNode[] = tasks.map((task) => {
      const pos =
        task.graph_x != null && task.graph_y != null
          ? { x: task.graph_x, y: task.graph_y + offsetY }
          : (() => {
              const base = fallback[task.id] ?? { x: 0, y: 0 };
              return { x: base.x, y: base.y + offsetY };
            })();
      const assignment = assignmentByTask.get(task.id);
      const member = assignment ? memberById.get(assignment.member_id) : undefined;
      // Friendly label from the fleet snapshot; fall back to the localized
      // "assigned" pill if the member can't be resolved (still better than a uuid).
      const friendly = memberShortLabel(member);
      const taskKind = normalizeTaskKind(task.kind);
      const isSynthesis = taskKind === 'synthesis';
      const isVerify = taskKind === 'verify';
      const isJudge = taskKind === 'judge';
      const isLoop = taskKind === 'loop';
      const groupLabel = parseGroupLabel(task.pattern_config);
      const groupHue = groupLabel ? hueByGroup.get(groupLabel) : undefined;
      return {
        id: task.id,
        type: 'task',
        position: pos,
        // Size hints so the MiniMap can render this node's rect (see
        // MINIMAP_NODE_W/H). `initialWidth/Height` give react-flow a non-zero
        // size for the minimap WITHOUT pinning the rendered card's DOM size, so
        // the card keeps its `w-220px` + auto-height on the main canvas.
        initialWidth: MINIMAP_NODE_W,
        initialHeight: MINIMAP_NODE_H,
        data: {
          title: task.title || t('orchestrator.run.detail.untitledTask'),
          status: task.status,
          statusLabel: t(`orchestrator.run.task.status.${task.status}`, {
            defaultValue: t('orchestrator.run.status.unknown'),
          }),
          kind: task.kind,
          synthesisLabel: isSynthesis ? t('orchestrator.run.kind.synthesis') : undefined,
          verifyLabel: isVerify ? t('orchestrator.run.kind.verify') : undefined,
          verifyVerdict: isVerify ? parseVerifyVerdict(task.output_summary) : undefined,
          verifyVerdictLabels: isVerify
            ? {
                pass: t('orchestrator.run.verdict.pass'),
                fail: t('orchestrator.run.verdict.fail'),
                pending: t('orchestrator.run.verdict.pending'),
              }
            : undefined,
          judgeLabel: isJudge ? t('orchestrator.run.kind.judge') : undefined,
          judgeWinner: isJudge ? parseJudgeWinner(task.output_summary) : undefined,
          judgeWinnerLabels: isJudge
            ? {
                winner: t('orchestrator.run.judge.winner'),
                none: t('orchestrator.run.judge.none'),
                pending: t('orchestrator.run.judge.pending'),
              }
            : undefined,
          loopLabel: isLoop ? t('orchestrator.run.kind.loop') : undefined,
          loopState: isLoop ? parseLoopState(task.output_summary) : undefined,
          loopStateLabels: isLoop
            ? {
                done: t('orchestrator.run.loop.done'),
                failed: t('orchestrator.run.loop.failed'),
                iterating: t('orchestrator.run.loop.iterating'),
              }
            : undefined,
          groupLabel,
          groupHue,
          groupChipLabel: groupLabel
            ? t('orchestrator.run.kind.fanout', { label: groupLabel })
            : undefined,
          memberId: assignment?.member_id,
          chipLabel: assignment ? (friendly ?? t('orchestrator.run.detail.assigned')) : undefined,
          memberLogo: memberLogo(member),
          locked: assignment?.locked ?? false,
          attempt: task.attempt,
          tokens: task.tokens,
          tokensLabel: t('orchestrator.run.node.tokens'),
          onOpen: () => handleOpenTask(task),
        },
      };
    });

    // BACKWARD COMPAT: without `onOpenMain` we return the task nodes untouched —
    // identical array shape, positions, and ordering as before this prop existed.
    if (!withMain || onOpenMain == null) return taskNodes;

    // Center the main node horizontally over the in-degree-0 root tasks' top row
    // (the layer that was originally at y=0, now shifted to y=MAIN_ROW_OFFSET).
    const rootIds = computeRootTaskIds(tasks, deps);
    const topRowXs = taskNodes
      .filter((n) => rootIds.has(n.id) && n.position.y === offsetY)
      .map((n) => n.position.x);
    // Fall back to every node's x if the root row can't be isolated (e.g. a
    // dependency cycle left no node at the top row) so the main node still
    // centers sensibly instead of snapping to x=0.
    const xsForCenter = topRowXs.length > 0 ? topRowXs : taskNodes.map((n) => n.position.x);
    const minX = Math.min(...xsForCenter);
    const maxX = Math.max(...xsForCenter);
    const mainX = (minX + maxX) / 2;

    const mainNode: MainFlowNode = {
      id: MAIN_NODE_ID,
      type: 'main',
      position: { x: mainX, y: 0 },
      initialWidth: MINIMAP_NODE_W,
      initialHeight: MINIMAP_NODE_H,
      data: {
        label: t('orchestrator.run.canvas.mainNode', { defaultValue: 'main · 主 agent' }),
        active: mainActive ?? false,
        onOpen: onOpenMain,
      },
    };
    return [mainNode, ...taskNodes];
  }, [detail?.tasks, detail?.deps, assignmentByTask, memberById, handleOpenTask, t, onOpenMain, mainActive]);

  const edges = useMemo<Edge[]>(() => {
    const tasks = detail?.tasks ?? [];
    const deps = detail?.deps ?? [];
    const statusById = new Map(tasks.map((task) => [task.id, task.status]));
    const depEdges = deps.map((dep) => {
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

    // BACKWARD COMPAT: without `onOpenMain` the edges are exactly the dependency
    // edges as before — no main→root wiring.
    if (onOpenMain == null || tasks.length === 0) return depEdges;

    // Connect the synthetic main node down to each in-degree-0 root task. Reuse
    // the resting (non-running) edge style — `var(--border-base)` via flowColors'
    // theme — so the main→root links read as the same calm dependency layer.
    const rootIds = computeRootTaskIds(tasks, deps);
    const mainEdges: Edge[] = [];
    for (const rootId of rootIds) {
      mainEdges.push({
        id: `e-main-${rootId}`,
        source: MAIN_NODE_ID,
        target: rootId,
        animated: false,
        style: { stroke: 'var(--border-base)', strokeWidth: 1.5 },
      });
    }
    return [...mainEdges, ...depEdges];
  }, [detail?.tasks, detail?.deps, onOpenMain]);

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
            nodeTypes={onOpenMain != null ? NODE_TYPES_WITH_MAIN : NODE_TYPES}
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
              position='top-right'
              maskColor={flowColors.minimapMask}
              style={{ background: flowColors.minimapBg, border: `1px solid ${flowColors.minimapStroke}` }}
              nodeColor={(n) => miniMapNodeColor(String((n.data as { status?: string }).status ?? ''), theme)}
              nodeStrokeWidth={2}
            />
          </ReactFlow>
        )}
      </div>
    </div>
  );
};

export default DagCanvas;
