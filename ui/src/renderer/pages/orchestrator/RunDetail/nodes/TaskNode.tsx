/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';
import { Handle, Position, type Node, type NodeProps } from '@xyflow/react';
import { Branch, CheckOne, CloseOne, Gavel, Help, Lightning, Lock, Merge, Refresh, Shield, Trophy } from '@icon-park/react';

/** Task status → theme-var color + a slow-pulse hint for the running state. */
export interface TaskStatusMeta {
  /** CSS color expression (theme var). */
  color: string;
  /** Whether the status dot should pulse (running). */
  pulse: boolean;
}

/**
 * Map a task status string to its on-brand color. Statuses come straight off
 * the wire (`TRunTask.status`), so unknown values fall back to a muted tone.
 *
 * pending → tertiary text · running → brand primary (pulsing) · done → success
 * · failed → danger · needs_review → warning · skipped → muted.
 */
export function taskStatusMeta(status: string): TaskStatusMeta {
  switch (status) {
    case 'running':
      return { color: 'rgb(var(--primary-6))', pulse: true };
    case 'done':
    case 'completed':
      return { color: 'var(--success)', pulse: false };
    case 'failed':
    case 'error':
      return { color: 'var(--danger)', pulse: false };
    case 'needs_review':
    case 'blocked':
      return { color: 'var(--warning)', pulse: false };
    case 'skipped':
    case 'cancelled':
      return { color: 'var(--text-disabled)', pulse: false };
    case 'pending':
    default:
      return { color: 'var(--bg-6)', pulse: false };
  }
}

/** The synthesis task kind — a node that merges its upstream tasks' outputs into
 * a final result. Every other (or unknown) `kind` renders as a plain agent node
 * with zero visual change, so the common case is untouched. */
export const TASK_KIND_SYNTHESIS = 'synthesis';

/** The verify task kind — a synchronous aggregator that tallies its skeptic
 * dependencies' pass/fail votes into a single verdict (written to its
 * `output_summary`) and gates downstream on a FAIL. Renders a shield badge + a
 * pass/fail verdict pill. Unknown kinds collapse to `'agent'` (no badge). */
export const TASK_KIND_VERIFY = 'verify';

/** The judge task kind — a synchronous aggregator that tallies N judges' ballots
 * over M candidates and writes a WINNER marker to its `output_summary`. Renders a
 * gavel badge + a winner pill (the picked candidate, or a neutral "no winner" /
 * "judging…" state). Unknown kinds collapse to `'agent'` (no badge). */
export const TASK_KIND_JUDGE = 'judge';

/** The loop task kind — a synchronous controller that iterates a body task
 * (bounded by `max_iter`) and writes a LOOP marker to its `output_summary` on
 * stop. Renders a refresh/cycle badge + an iteration/stop-state pill (done /
 * failed / neutral "iterating…"). The body's per-iteration count surfaces via
 * the existing `attempt` retry badge. Unknown kinds collapse to `'agent'`. */
export const TASK_KIND_LOOP = 'loop';

/**
 * Normalize a raw `TRunTask.kind` defensively. The wire value defaults to
 * `'agent'` on the backend, but legacy / malformed values must never crash the
 * canvas — anything we don't recognize collapses to `'agent'` (no badge).
 */
export function normalizeTaskKind(
  kind: string | null | undefined
): 'agent' | 'synthesis' | 'verify' | 'judge' | 'loop' {
  if (kind === TASK_KIND_SYNTHESIS) return 'synthesis';
  if (kind === TASK_KIND_VERIFY) return 'verify';
  if (kind === TASK_KIND_JUDGE) return 'judge';
  if (kind === TASK_KIND_LOOP) return 'loop';
  return 'agent';
}

/** Brand-tinted accent for the synthesis badge — intentionally distinct from the
 * status palette (success/danger/warning/primary) so a synthesis node reads as a
 * structural role, not a status. Defined in every theme preset. */
const SYNTH_ACCENT = 'var(--brand)';

/** Accent for the verify-kind badge — uses the primary brand tone so the badge
 * itself reads as a structural role (the verdict pill carries the success/danger
 * semantics separately, so the badge must NOT borrow a status color). */
const VERIFY_ACCENT = 'rgb(var(--primary-6))';

/** Accent for the judge-kind badge — uses the brand tone (same family as the
 * synthesis badge) so the gavel reads as a structural aggregator role. The
 * winner pill carries the success/neutral semantics separately, so the badge
 * must NOT borrow a status color. Defined in every theme preset. */
const JUDGE_ACCENT = 'var(--brand)';

/** Accent for the loop-kind badge — uses the brand tone (same structural family
 * as the synthesis / judge badges) so the cycle glyph reads as a controller
 * role. The iteration/stop pill carries the success/danger/neutral semantics
 * separately, so the badge must NOT borrow a status color. Defined in every
 * theme preset. */
const LOOP_ACCENT = 'var(--brand)';

/** A parsed judge result, ready for the winner pill. `winner === null` means the
 * marker said `none`, was absent, or was unparseable (or the node hasn't settled
 * yet) → neutral "no winner / judging…" state. */
export interface JudgeWinner {
  /** 0-based index of the winning candidate, or `null` for no-winner / pending. */
  winner: number | null;
  /** Aggregation policy parsed from the marker (`mean` | `borda`), else null. */
  aggregate: 'mean' | 'borda' | null;
  /** Judge tally string like `"2/3"` when present in the marker, else null. */
  judges: string | null;
}

/** A parsed verify verdict, ready for the pill. `pass === null` means the marker
 * was absent or unparseable (or the node hasn't settled yet) → neutral state. */
export interface VerifyVerdict {
  /** true = PASS · false = FAIL · null = unknown / still verifying. */
  pass: boolean | null;
  /** Tally string like `"2/3"` when present in the marker, else null. */
  tally: string | null;
}

/** A parsed loop controller state, ready for the pill. `state === null` means the
 * marker was absent / a transient `LOOP-STATE:` line / unparseable (or the loop
 * is still iterating) → neutral "iterating…" state. */
export interface LoopState {
  /** 'done' = loop stopped successfully · 'failed' = body failed · null = still
   * iterating / unparseable. */
  state: 'done' | 'failed' | null;
  /** Stop reason from the marker (`max_iter` | `predicate` | `dry` |
   * `body_failed` | `no_body`), else null. */
  reason: string | null;
  /** Completed iteration count parsed from the marker, else null. */
  iterations: number | null;
  /** The configured hard cap (`max_iter`) parsed from the marker, else null. */
  maxIter: number | null;
}

/** The data payload DagCanvas attaches to each task node. */
export interface TaskNodeData extends Record<string, unknown> {
  title: string;
  status: string;
  statusLabel: string;
  /** Raw task kind off the wire (`'agent'` default | `'synthesis'`). Rendered
   * defensively via {@link normalizeTaskKind}; unknown → agent (no badge). */
  kind?: string;
  /** 入场动画序号（需求2）：错峰淡入上浮的 `--dag-i` 延迟因子（DagCanvas 已 cap）。 */
  enterIndex?: number;
  /** Localized "synthesis" label for the kind badge (computed in DagCanvas so the
   * node stays free of i18n wiring). */
  synthesisLabel?: string;
  /** Localized "verify" label for the verify-kind badge (computed in DagCanvas). */
  verifyLabel?: string;
  /** Localized "judge" label for the judge-kind badge (computed in DagCanvas). */
  judgeLabel?: string;
  /** Localized "loop" label for the loop-kind badge (computed in DagCanvas). */
  loopLabel?: string;
  /** Parsed pass/fail verdict for a `verify` node (from its `output_summary`).
   * Present only for verify-kind nodes; rendered as a verdict pill. A `pass` of
   * `null` shows the neutral "verifying…" state (marker absent / unparseable). */
  verifyVerdict?: VerifyVerdict;
  /** Localized labels for the verdict pill — pass / fail / pending text. */
  verifyVerdictLabels?: { pass: string; fail: string; pending: string };
  /** Parsed winner for a `judge` node (from its `output_summary`). Present only
   * for judge-kind nodes; rendered as a winner pill. A `winner` of `null` shows
   * the neutral "no winner / judging…" state (marker absent / `none` / bad). */
  judgeWinner?: JudgeWinner;
  /** Localized labels for the winner pill — winner / none / pending text. */
  judgeWinnerLabels?: { winner: string; none: string; pending: string };
  /** Parsed iteration/stop state for a `loop` controller node (from its
   * `output_summary`). Present only for loop-kind nodes; rendered as a state
   * pill. A `state` of `null` shows the neutral "iterating…" state (marker
   * absent / transient `LOOP-STATE:` line / unparseable). */
  loopState?: LoopState;
  /** Localized labels for the loop pill — done / failed / iterating text. */
  loopStateLabels?: { done: string; failed: string; iterating: string };
  /** Fan-out group label parsed from `pattern_config` (`{"group":"<label>"}`).
   * Present only for sibling tasks the planner fanned out in parallel. */
  groupLabel?: string;
  /** Per-group hue (HSL degrees) so every sibling in a fan-out group shares one
   * tint — a calm, deterministic color derived from the group label. */
  groupHue?: number;
  /** Localized "fan-out: {{label}}" text for the group chip. */
  groupChipLabel?: string;
  /** Assigned fleet member id (raw uuid — used only for the chip tooltip). */
  memberId?: string;
  /** Friendly chip label resolved from the run's fleet snapshot:
   * agent id (+ model). Falls back to a localized "assigned" when the member
   * can't be resolved against `fleet_members`. */
  chipLabel?: string;
  /** Logo url for the assigned agent (resolved from agent_id), if any. */
  memberLogo?: string | null;
  attempt: number;
  /** Whether this assignment is locked (pinned against auto-routing). */
  locked?: boolean;
  /** Per-task token usage off the wire (`TRunTask.tokens`). Rendered as a compact
   * chip ONLY when it's a positive number; null / 0 / not-yet-run → no chip. */
  tokens?: number | null;
  /** Localized terse "tok" label for the token chip (computed in DagCanvas so the
   * node stays free of i18n wiring). */
  tokensLabel?: string;
  /** 审批模式（需求5，迁移 030）：节点挂起的决策问题原文。在场时渲染醒目的提问
   * 徽标（缓脉冲）+ 琥珀 ring，点击节点即投影进 worker 会话作答。 */
  pendingQuestion?: string;
  /** Localized "待作答" text for the question badge (computed in DagCanvas). */
  questionLabel?: string;
  /** Click handler — opens the task inspector / transcript panel. */
  onOpen: () => void;
}

/** Strongly-typed node alias so NodeProps narrows `data` for us. */
export type TaskFlowNode = Node<TaskNodeData, 'task'>;

/** Shared pill class（meta 行小胶囊的公共原子类，样式差异走内联主题色）。 */
const PILL_CLASS = 'inline-flex shrink-0 items-center gap-3px rd-100px px-6px py-2px text-10px font-600 leading-none';

/** Tinted pill style（统一「主题色 14% 底 + 32% 描边」公式，全部主题变量）。 */
function tintedPill(tone: string): React.CSSProperties {
  return {
    color: tone,
    background: `color-mix(in srgb, ${tone} 14%, transparent)`,
    border: `1px solid color-mix(in srgb, ${tone} 32%, transparent)`,
  };
}

/** Neutral pill style（未定态：verifying…/judging…/iterating…）。 */
const NEUTRAL_PILL: React.CSSProperties = {
  color: 'var(--text-secondary)',
  background: 'var(--color-fill-1)',
  border: '1px solid var(--border-light)',
};

/**
 * TaskNode — a custom react-flow node rendering one DAG task as an on-brand
 * card（需求2 精美化）: a status accent strip + shimmer while running, a title
 * row (pulsing status dot + title + kind badge), a meta row (status label +
 * verdict/winner/loop pills + assignment chip + retry badge + fan-out chip +
 * token chip), and — in approval mode — a prominent question badge. Hover lift
 * / press / selection ring / running glow all live in `dag-canvas.css` keyed by
 * data attributes; the node only feeds `--node-accent` and state flags. Theme
 * variables only (no hardcoded hex); source/target handles anchor the edges.
 */
function TaskNodeImpl({ data, selected }: NodeProps<TaskFlowNode>) {
  const meta = taskStatusMeta(data.status);
  const kind = normalizeTaskKind(data.kind);
  const isSynthesis = kind === 'synthesis';
  const isVerify = kind === 'verify';
  const isJudge = kind === 'judge';
  const isLoop = kind === 'loop';
  const hasQuestion = Boolean(data.pendingQuestion);
  // A fan-out group needs a label AND a resolved hue; either missing → no group
  // affordance (defensive against half-parsed config).
  const inGroup = data.groupLabel != null && data.groupHue != null;
  const groupColor = inGroup ? `hsl(${data.groupHue}, 62%, 55%)` : null;

  return (
    <div
      role='button'
      tabIndex={0}
      aria-label={`${data.title} · ${data.statusLabel}`}
      onClick={data.onOpen}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          data.onOpen();
        }
      }}
      className='nomi-dag-card nomi-dag-enter group relative flex w-220px cursor-pointer select-none flex-col gap-8px rd-12px px-14px pb-12px pt-14px outline-none'
      data-status={data.status}
      data-selected={selected ? 'true' : undefined}
      data-question={hasQuestion ? 'true' : undefined}
      data-grouped={inGroup ? 'true' : undefined}
      style={
        {
          '--node-accent': meta.color,
          '--group-accent': groupColor ?? 'transparent',
          '--dag-i': data.enterIndex ?? 0,
        } as React.CSSProperties
      }
    >
      {/* 顶部状态条纹：常态 2px 主题色；running 时叠加 shimmer 进度光带。 */}
      <span className='nomi-dag-accent' aria-hidden='true'>
        {meta.pulse && <span className='nomi-dag-shimmer' />}
      </span>

      {/* Incoming-dependency anchor (top) */}
      <Handle
        type='target'
        position={Position.Top}
        isConnectable={false}
        style={{ width: 7, height: 7, background: 'var(--bg-5)', border: 'none' }}
      />

      {/* Title row: status dot + task title (+ kind badge, right-aligned) */}
      <div className='flex items-start gap-8px'>
        <span
          className={`mt-4px size-9px shrink-0 rd-full ${meta.pulse ? 'nomi-dag-pulse' : ''}`}
          style={{ background: meta.color, boxShadow: `0 0 0 3px color-mix(in srgb, ${meta.color} 20%, transparent)` }}
        />
        <span className='min-w-0 flex-1 text-13px font-600 leading-18px text-t-primary line-clamp-2'>
          {data.title}
        </span>
        {isSynthesis && (
          <span className={`nomi-dag-kind-badge ${PILL_CLASS}`} style={tintedPill(SYNTH_ACCENT)} title={data.synthesisLabel}>
            <Merge theme='outline' size='10' strokeWidth={4} className='line-height-0' />
            {data.synthesisLabel}
          </span>
        )}
        {isVerify && (
          <span className={`nomi-dag-kind-badge ${PILL_CLASS}`} style={tintedPill(VERIFY_ACCENT)} title={data.verifyLabel}>
            <Shield theme='outline' size='10' strokeWidth={4} className='line-height-0' />
            {data.verifyLabel}
          </span>
        )}
        {isJudge && (
          <span className={`nomi-dag-kind-badge ${PILL_CLASS}`} style={tintedPill(JUDGE_ACCENT)} title={data.judgeLabel}>
            <Gavel theme='outline' size='10' strokeWidth={4} className='line-height-0' />
            {data.judgeLabel}
          </span>
        )}
        {isLoop && (
          <span className={`nomi-dag-kind-badge ${PILL_CLASS}`} style={tintedPill(LOOP_ACCENT)} title={data.loopLabel}>
            <Refresh theme='outline' size='10' strokeWidth={4} className='line-height-0' />
            {data.loopLabel}
          </span>
        )}
      </div>

      {/* Meta row: status label + verdict pill + assignment chip + retry badge + fan-out group chip */}
      <div className='flex flex-wrap items-center gap-6px'>
        <span className='text-11px font-500 leading-none' style={{ color: meta.color }}>
          {data.statusLabel}
        </span>
        {isVerify && data.verifyVerdict && (() => {
          const { pass, tally } = data.verifyVerdict;
          const labels = data.verifyVerdictLabels;
          // pass===true → success · pass===false → danger · pass===null → neutral.
          const tone = pass === true ? 'var(--success)' : pass === false ? 'var(--danger)' : null;
          const text =
            pass === true
              ? `${labels?.pass ?? ''}${tally ? ` ${tally}` : ''}`
              : pass === false
                ? `${labels?.fail ?? ''}${tally ? ` ${tally}` : ''}`
                : (labels?.pending ?? '');
          return (
            <span className={PILL_CLASS} style={tone ? tintedPill(tone) : NEUTRAL_PILL} title={text}>
              {pass === true && (
                <CheckOne theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
              )}
              {pass === false && (
                <CloseOne theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
              )}
              <span className='truncate'>{text}</span>
            </span>
          );
        })()}
        {isJudge && data.judgeWinner && (() => {
          const { winner, judges } = data.judgeWinner;
          const labels = data.judgeWinnerLabels;
          const hasWinner = winner !== null;
          const text = hasWinner
            ? `${labels?.winner ?? ''} #${winner}${judges ? ` · ${judges}` : ''}`
            : (labels?.none ?? labels?.pending ?? '');
          return (
            <span className={PILL_CLASS} style={hasWinner ? tintedPill('var(--success)') : NEUTRAL_PILL} title={text}>
              {hasWinner && (
                <Trophy theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
              )}
              <span className='truncate'>{text}</span>
            </span>
          );
        })()}
        {isLoop && data.loopState && (() => {
          const { state, reason, iterations, maxIter } = data.loopState;
          const labels = data.loopStateLabels;
          const tone = state === 'done' ? 'var(--success)' : state === 'failed' ? 'var(--danger)' : null;
          // DONE shows the iteration tally (N/M) + reason; FAILED shows the reason;
          // null shows the neutral "iterating…" label. Reason is best-effort extra.
          const reasonSuffix = reason ? ` · ${reason}` : '';
          const text =
            state === 'done'
              ? `${labels?.done ?? ''}${iterations != null && maxIter != null ? ` ${iterations}/${maxIter}` : ''}${reasonSuffix}`
              : state === 'failed'
                ? `${labels?.failed ?? ''}${reasonSuffix}`
                : (labels?.iterating ?? '');
          return (
            <span className={PILL_CLASS} style={tone ? tintedPill(tone) : NEUTRAL_PILL} title={text}>
              {state === 'done' && (
                <CheckOne theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
              )}
              {state === 'failed' && (
                <CloseOne theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
              )}
              {state === null && (
                <Refresh theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
              )}
              <span className='truncate'>{text}</span>
            </span>
          );
        })()}
        {data.chipLabel && (
          <span
            className='inline-flex max-w-[150px] items-center gap-3px rd-100px px-6px py-2px text-10px leading-none text-t-secondary'
            style={{ background: 'var(--fill-0)', border: '1px solid var(--border-light)' }}
            title={data.memberId}
          >
            {data.memberLogo ? (
              <img src={data.memberLogo} alt='' className='size-10px shrink-0 object-contain' />
            ) : (
              <span
                className='size-5px shrink-0 rd-full'
                style={{ background: 'rgb(var(--primary-6))' }}
              />
            )}
            <span className='truncate'>{data.chipLabel}</span>
            {data.locked && (
              <Lock theme='outline' size='9' strokeWidth={4} className='shrink-0 text-t-tertiary' />
            )}
          </span>
        )}
        {data.attempt > 1 && (
          <span
            className='inline-flex items-center rd-100px px-6px py-2px text-10px leading-none'
            style={{ background: 'color-mix(in srgb, var(--warning) 16%, transparent)', color: 'var(--warning)' }}
          >
            ×{data.attempt}
          </span>
        )}
        {inGroup && groupColor && (
          <span
            className='inline-flex max-w-[150px] items-center gap-3px rd-100px px-6px py-2px text-10px font-500 leading-none'
            style={tintedPill(groupColor)}
            title={data.groupChipLabel}
          >
            <Branch theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
            <span className='truncate'>{data.groupChipLabel}</span>
          </span>
        )}
        {typeof data.tokens === 'number' && data.tokens > 0 && (
          <span
            className='inline-flex shrink-0 items-center gap-3px rd-100px px-6px py-2px text-10px leading-none tabular-nums text-t-tertiary'
            style={{ background: 'var(--fill-0)', border: '1px solid var(--border-light)' }}
            title={`${data.tokens.toLocaleString()} ${data.tokensLabel ?? ''}`.trim()}
          >
            <Lightning theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
            <span className='truncate'>{data.tokens.toLocaleString()}</span>
          </span>
        )}
      </div>

      {/* 审批模式（需求5）：挂起的决策问题 → 醒目提问条（缓脉冲徽标 + 问题预览）。
          点击整卡即投影进 worker 会话作答。 */}
      {hasQuestion && (
        <div
          className='nomi-dag-question flex items-start gap-6px rd-8px px-8px py-6px'
          title={data.pendingQuestion}
        >
          <span className='nomi-dag-question-pulse mt-1px shrink-0 line-height-0' style={{ color: 'var(--warning)' }}>
            <Help theme='filled' size='13' />
          </span>
          <span className='min-w-0 flex-1 text-11px leading-15px line-clamp-2' style={{ color: 'var(--warning)' }}>
            {data.questionLabel && <b className='mr-4px'>{data.questionLabel}</b>}
            {data.pendingQuestion}
          </span>
        </div>
      )}

      {/* Outgoing-dependency anchor (bottom) */}
      <Handle
        type='source'
        position={Position.Bottom}
        isConnectable={false}
        style={{ width: 7, height: 7, background: 'var(--bg-5)', border: 'none' }}
      />
    </div>
  );
}

export default React.memo(TaskNodeImpl);
