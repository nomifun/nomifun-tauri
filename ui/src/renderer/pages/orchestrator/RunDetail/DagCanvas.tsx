/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ReactFlow, Background, BackgroundVariant, Controls, MiniMap, type Edge } from '@xyflow/react';
import type { ReactFlowInstance } from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import './dag-canvas.css';
import { Branch } from '@icon-park/react';
import { Spin } from '@arco-design/web-react';
import type { TAssignment, TFleetMember, TRunDetail, TRunTask } from '@/common/types/orchestrator/orchestratorTypes';
import type { LeadThinkingState } from '../useLeadThinking';
import { layoutDag } from './layoutDag';
import { memberLogo, memberShortLabel } from './memberLabel';
import RolePrecipitationPanel from './RolePrecipitationPanel';
import TaskNode, { normalizeTaskKind, type TaskFlowNode, type JudgeWinner, type LoopState, type VerifyVerdict } from './nodes/TaskNode';

/** Stable nodeTypes ref so react-flow doesn't warn about a new object each
 * render. Task-only: the synthetic main/lead node was removed (需求5 —— main
 * agent 贯穿全程、非首节点，不再在画布上呈现；其产出由会话内回执转述)。 */
const NODE_TYPES = { task: TaskNode } as const;

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
  /** The run detail — SINGLE SOURCE (需求3 性能): the canvas no longer holds its
   * own `useRunLive` subscription; the consumer (OrchestrationTopPanel) passes
   * the context's live detail down, so one run has exactly ONE WS→REST loop. */
  detail: TRunDetail | null;
  /** First-load indicator from the shared live hook. */
  loading: boolean;
  /** Shared refetch (threaded into {@link OpenTaskPayload}). */
  refetch: () => Promise<void>;
  onOpenTask: (payload: OpenTaskPayload) => void;
  /** Lead planning stream — drives the planning placeholder's live narration
   * (phase keys + reasoning tail) so the user SEES the design process instead of
   * a static "规划中" (需求3 体感). Optional: absent = static placeholder. */
  leadThinking?: LeadThinkingState;
  /** The currently-projected task id (the conversation's content-area projection
   * state). The matching task node renders in its `selected` state so the canvas
   * mirrors EXACTLY what the content area is showing — clicking a node feels
   * responsive instead of "nothing happened". `null`/absent → no task selected.
   * Selection is driven SOLELY by this prop — react-flow's own
   * interaction-selection is turned off (`elementsSelectable=false`) so the
   * canvas never fights the projection state with a discarded, flickering
   * internal selection. */
  activeTaskId?: string | null;
}

/**
 * DagCanvas — the visual centerpiece of「agent 集群」. Renders a run's task DAG
 * as an interactive react-flow graph: each task is a custom {@link TaskNode},
 * each `blocker → blocked` dependency is an edge (animated while the downstream
 * task runs). Live data arrives via props from the shared orchestration context
 * (single `useRunLive` instance); clicking a node projects the worker
 * conversation into the content area through `onOpenTask`.
 *
 * The synthetic lead/main agent node was REMOVED (需求5): the main agent is a
 * through-line, not a first node — its narration lives in receipts and the
 * canvas-side progress summary, so the canvas shows ONLY the real task DAG.
 *
 * Positions prefer the task's persisted `graph_x/graph_y` and otherwise fall
 * back to a topological auto-layout ({@link layoutDag}). react-flow's JS-side
 * colors (MiniMap mask, Background dots) can't read CSS vars, so we mirror the
 * `data-theme` attribute into `colorMode` + resolved colors via a MutationObserver
 * (template: MermaidBlock).
 */
const DagCanvas: React.FC<DagCanvasProps> = ({
  runId,
  detail,
  loading,
  refetch,
  onOpenTask,
  leadThinking,
  activeTaskId,
}) => {
  const { t, i18n } = useTranslation();

  // The static `fitView` prop fits ONCE at initial mount. If the canvas ever
  // mounts inside a COLLAPSED / ~0-size
  // container, that initial fit happens against a zero viewport and leaves
  // the nodes tiny in a corner; when the container later expands, react-flow
  // never re-fits on its own. We capture the instance via `onInit` and re-run
  // `fitView()` whenever the wrapper transitions from ~0 → a real size (i.e.
  // becomes visible). The standalone orchestrator page is sized at mount, so it
  // simply gets a harmless extra refit. `wasVisibleRef` guards against thrashing
  // by only firing on the 0→visible edge.
  const rfRef = useRef<ReactFlowInstance<TaskFlowNode, Edge> | null>(null);
  const flowWrapRef = useRef<HTMLDivElement | null>(null);
  const wasVisibleRef = useRef(false);
  // Node-object identity cache (keyed by node id → {signature, object}). Reusing
  // the SAME object reference for an unchanged node lets react-flow's
  // `adoptUserNodes` hit its reference-equality reuse branch and PRESERVE that
  // node's already-measured `handleBounds` — instead of rebuilding a fresh
  // object every render (on click / live refetch), which resets handleBounds and
  // (with the previous declared-handles hack) permanently broke the edge
  // geometry until a remount. See the note above `buildNodes` below.
  const nodeCacheRef = useRef<Map<string, { sig: string; node: TaskFlowNode }>>(new Map());
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
    if (tasks.length === 0) {
      nodeCacheRef.current.clear();
      return [];
    }
    const fallback = layoutDag(tasks, deps);
    // Resolve each fan-out group's shared hue once (deterministic from the label)
    // so every sibling in a group lands on the same tint.
    const hueByGroup = new Map<string, number>();
    for (const task of tasks) {
      const group = parseGroupLabel(task.pattern_config);
      if (group && !hueByGroup.has(group)) hueByGroup.set(group, hueForGroup(group));
    }
    // ROOT-CAUSE FIX for "画布连线丢失 / 点击后连线乱跳".
    //
    // This canvas drives `nodes` as a controlled prop and rebuilds a FRESH object
    // for every node on each render (click → activeTaskId change, or live
    // refetch). react-flow's `adoptUserNodes` reuses a node's internals ONLY when
    // the object reference is identical (`userNode === prev.internals.userNode`);
    // a fresh object takes the rebuild branch, which resets `internals.measured`
    // and recomputes `handleBounds` from the userNode. The prior fixes made this
    // worse: declaring `handles` (+ `initialWidth`) kept `isInitialized`
    // permanently true, so the NodeWrapper's ResizeObserver NEVER re-measured the
    // DOM after a click — the edge endpoints froze at a stale/fallback height and
    // only a remount ("refresh") restored them.
    //
    // Fix = (B) DON'T declare `handles` / feed `measured` back, so a rebuilt
    // node's `handleBounds` legitimately resets to undefined → `isInitialized`
    // flips false → react-flow unobserves+re-observes and re-measures the real DOM
    // (exactly the mount/refresh path) → accurate, self-healing. Plus (C) reuse
    // the SAME object reference for any node whose render-relevant content is
    // unchanged, so react-flow keeps its already-measured handleBounds untouched —
    // meaning only the one node whose selection actually flipped re-measures, and
    // every other edge stays rock-steady across clicks.
    const cache = nodeCacheRef.current;
    const reuse = (id: string, sig: string, build: () => TaskFlowNode): TaskFlowNode => {
      const cached = cache.get(id);
      if (cached && cached.sig === sig) return cached.node;
      const node = build();
      cache.set(id, { sig, node });
      return node;
    };
    const taskNodes: TaskFlowNode[] = tasks.map((task, index) => {
      const pos =
        task.graph_x != null && task.graph_y != null
          ? { x: task.graph_x, y: task.graph_y }
          : (fallback[task.id] ?? { x: 0, y: 0 });
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
      const selected = activeTaskId != null && task.id === activeTaskId;
      const pendingQuestion = task.pending_question?.trim() ? task.pending_question : undefined;
      // 廉价签名（需求3 性能）：手工拼渲染相关字段，替代整对象 JSON.stringify。
      // 覆盖 data 的全部“信息源”字段——status/title/kind/output_summary（verify/
      // judge/loop pill 全由它派生）/分组/成员/锁/attempt/tokens/提问/选中/坐标/
      // 入场序号/语言（本地化文案随语言切换必须重建）。新增 TaskNodeData 字段时
      // 必须同步加入这里，否则该字段变化不会触发节点对象重建（渲染不更新）。
      const sig = [
        i18n.language,
        task.status,
        task.title,
        task.kind,
        task.output_summary ?? '',
        groupLabel ?? '',
        assignment?.member_id ?? '',
        friendly ?? '',
        memberLogo(member) ?? '',
        assignment?.locked ? '1' : '0',
        task.attempt,
        task.tokens ?? '',
        pendingQuestion ?? '',
        selected ? '1' : '0',
        pos.x,
        pos.y,
        index,
      ].join('');
      return reuse(task.id, sig, () => ({
        id: task.id,
        type: 'task',
        // react-flow forwards `selected` to NodeProps.selected (TaskNode's ring).
        selected,
        position: pos,
        // Size hints so the MiniMap can render this node's rect (see
        // MINIMAP_NODE_W/H) and `nodeHasDimensions` stays true; the rendered card
        // keeps its `w-220px` + auto-height (these never pin the DOM size).
        initialWidth: MINIMAP_NODE_W,
        initialHeight: MINIMAP_NODE_H,
        data: {
          title: task.title || t('orchestrator.run.detail.untitledTask'),
          status: task.status,
          statusLabel: t(`orchestrator.run.task.status.${task.status}`, {
            defaultValue: t('orchestrator.run.status.unknown'),
          }),
          kind: task.kind,
          // 入场动画序号（需求2 精美化）：错峰淡入上浮，上限之外不再递增延迟。
          enterIndex: Math.min(index, 12),
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
          // 审批模式（需求5）：节点挂起的决策问题 → 提问徽标 + 琥珀 ring。
          pendingQuestion,
          questionLabel: pendingQuestion
            ? t('orchestrator.run.question.badge', { defaultValue: '待作答' })
            : undefined,
          onOpen: () => handleOpenTask(task),
        },
      }));
    });

    // 缓存清理（需求7 资源）：剔除已不在本轮 tasks 里的节点（replan/调整删除的
    // 任务），长会话下缓存不随历史节点无界增长。
    if (cache.size > taskNodes.length) {
      const liveIds = new Set(tasks.map((task) => task.id));
      for (const key of cache.keys()) {
        if (!liveIds.has(key)) cache.delete(key);
      }
    }
    return taskNodes;
  }, [detail?.tasks, detail?.deps, assignmentByTask, memberById, handleOpenTask, t, i18n.language, activeTaskId]);

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
        // 流光样式类（需求2）：下游 running 时边缘发亮流动，静止边保持淡雅。
        className: downstreamRunning ? 'nomi-dag-edge-live' : undefined,
        style: {
          stroke: downstreamRunning ? 'rgb(var(--primary-6))' : 'var(--border-base)',
          strokeWidth: downstreamRunning ? 2 : 1.5,
        },
      };
    });
  }, [detail?.tasks, detail?.deps]);

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
  // 规划叙事（需求3 体感）：把 leadThinking 的阶段 key 逐条点亮 + reasoning 尾巴，
  // 让用户在计划落地前就看见主 agent 的设计流程在推进。key→文案映射复用
  // orchestrator.run.thinking.phase.*（与 RunDecisionFeed 的 phaseNarration 同源）。
  const PHASE_I18N: Record<string, string> = {
    planning_started: 'orchestrator.run.thinking.phase.planningStarted',
    decomposing: 'orchestrator.run.thinking.phase.decomposing',
    assigning: 'orchestrator.run.thinking.phase.assigning',
    plan_ready: 'orchestrator.run.thinking.phase.planReady',
  };
  const phaseKeys = leadThinking?.phaseKeys ?? [];
  const reasoningTail = (leadThinking?.reasoning ?? '').trim().slice(-160);

  return (
    <div className='size-full min-h-0 flex flex-col'>
      {/* Role precipitation — when the run is done, suggest saving its used
          roles as assistants. Lives as a `shrink-0` sibling above the canvas so
          the react-flow region keeps its `flex-1 min-h-0` sizing intact. The
          panel renders nothing when there are no roles / all already exist. */}
      {(detail.run.status === 'completed' || detail.run.status === 'completed_with_failures') && (
        <RolePrecipitationPanel detail={detail} />
      )}

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
            {phaseKeys.length > 0 && (
              <div className='flex max-w-340px flex-col items-stretch gap-4px' aria-live='polite'>
                {phaseKeys.slice(-4).map((key) => (
                  <div key={key} className='nomi-dag-phase-line text-12px leading-18px text-t-secondary'>
                    {PHASE_I18N[key] ? t(PHASE_I18N[key]) : key}
                  </div>
                ))}
              </div>
            )}
            {leadThinking?.active && reasoningTail && (
              <div className='nomi-dag-reasoning-tail max-w-340px text-11px leading-16px text-t-tertiary'>
                {reasoningTail}
              </div>
            )}
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
            // Selection is driven entirely by `activeTaskId` (the projection
            // state), NOT by react-flow's interaction layer. Turning this off
            // stops react-flow from running its own select-on-click write that
            // we never persist (no `onNodesChange`) and would only get wiped on
            // the next controlled re-render — the source of the old "clicked but
            // nothing lights up / feels janky" behavior. Nodes stay clickable
            // (our inner `<div onClick>`) and draggable (pointer-events stay on
            // via `nodesDraggable`).
            elementsSelectable={false}
          >
            <Background variant={BackgroundVariant.Dots} gap={22} size={1.2} color={flowColors.dots} />
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
