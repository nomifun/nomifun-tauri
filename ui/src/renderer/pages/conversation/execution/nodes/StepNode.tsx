/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { Popover } from '@arco-design/web-react';
import React from 'react';
import { Handle, Position, useStore, type Node, type NodeProps } from '@xyflow/react';
import { Branch, CheckOne, CloseOne, Gavel, Help, Lightning, Merge, Refresh, Shield, Trophy } from '@icon-park/react';

/** Task status → theme-var color + a slow-pulse hint for the running state. */
export interface StepStatusMeta {
  /** CSS color expression (theme var). */
  color: string;
  /** Whether the status dot should pulse (running). */
  pulse: boolean;
}

/**
 * Map a canonical task status to its on-brand color. Unknown values fall back
 * to a muted tone.
 */
export function stepStatusMeta(status: string): StepStatusMeta {
  switch (status) {
    case 'running':
      return { color: 'rgb(var(--primary-6))', pulse: true };
    case 'completed':
      return { color: 'var(--success)', pulse: false };
    case 'failed':
      return { color: 'var(--danger)', pulse: false };
    case 'waiting_input':
      return { color: 'var(--warning)', pulse: false };
    case 'skipped':
    case 'cancelled':
      return { color: 'var(--text-disabled)', pulse: false };
    case 'pending':
    default:
      return { color: 'var(--bg-6)', pulse: false };
  }
}

/** The synthesis task mode merges its upstream tasks' outputs into
 * a final result. Every other (or unknown) value renders as a plain task
 * with zero visual change, so the common case is untouched. */
export const STEP_KIND_SYNTHESIS = 'synthesis';

/** The verify task kind — a synchronous aggregator that tallies its skeptic
 * dependencies' pass/fail votes into a single verdict (written to its
 * `output_summary`) and gates downstream on a FAIL. Renders a shield badge + a
 * pass/fail verdict pill. Unknown kinds collapse to `'agent'` (no badge). */
export const STEP_KIND_VERIFY = 'verify';

/** The judge task kind — a synchronous aggregator that tallies N judges' ballots
 * over M candidates and writes a WINNER marker to its `output_summary`. Renders a
 * gavel badge + a winner pill (the picked candidate, or a neutral "no winner" /
 * "judging…" state). Unknown kinds collapse to `'agent'` (no badge). */
export const STEP_KIND_JUDGE = 'judge';

/** The loop task kind — a synchronous controller that iterates a body task
 * (bounded by `max_iter`) and writes a LOOP marker to its `output_summary` on
 * stop. Renders a refresh/cycle badge + an iteration/stop-state pill (done /
 * failed / neutral "iterating…"). The body's per-iteration count surfaces via
 * the existing `attempt` retry badge. Unknown kinds collapse to `'agent'`. */
export const STEP_KIND_LOOP = 'loop';

/**
 * Normalize a task kind plus the synthesis display mode defensively. Unknown
 * values never crash the canvas and render as a plain task.
 */
export function normalizeStepKind(kind: string | null | undefined): 'agent' | 'synthesis' | 'verify' | 'judge' | 'loop' {
  if (kind === STEP_KIND_SYNTHESIS) return 'synthesis';
  if (kind === STEP_KIND_VERIFY) return 'verify';
  if (kind === STEP_KIND_JUDGE) return 'judge';
  if (kind === STEP_KIND_LOOP) return 'loop';
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
export interface StepNodeData extends Record<string, unknown> {
  title: string;
  spec?: string;
  outputSummary?: string;
  errorSummary?: string;
  status: string;
  statusLabel: string;
  /** Task kind or synthesis display mode. Unknown values render without a badge. */
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
  /** Localized "fan-out: {{label}}" text for the group chip. */
  groupChipLabel?: string;
  /** Assigned execution participant id, used only for the chip tooltip. */
  participantId?: string;
  /** Friendly chip label resolved from the execution participant snapshot. */
  chipLabel?: string;
  /** Logo url for the assigned participant, if any. */
  participantLogo?: string | null;
  roleLabel?: string;
  modelLabel?: string;
  attempt: number;
  /** Whether this assignment is locked (pinned against auto-routing). */
  locked?: boolean;
  /** Per-task token usage from the latest attempt. */
  tokens?: number | null;
  /** Localized terse "tok" label for the token chip (computed in DagCanvas so the
   * node stays free of i18n wiring). */
  tokensLabel?: string;
  /** Decision question raised by the latest attempt. */
  pendingQuestion?: string;
  /** Localized "待作答" text for the question badge (computed in DagCanvas). */
  questionLabel?: string;
  relationState?: 'idle' | 'focus' | 'related' | 'muted';
  upstreamLabels: string[];
  downstreamLabels: string[];
  quickLookLabels: {
    result: string;
    upstream: string;
    downstream: string;
    model: string;
    role: string;
    attempts: string;
    inspect: string;
  };
  onHoverChange?: (active: boolean) => void;
  /** Click handler — opens the task inspector / transcript panel. */
  onOpen: () => void;
}

/** Strongly-typed node alias so NodeProps narrows `data` for us. */
export type StepFlowNode = Node<StepNodeData, 'step'>;

type SemanticResult = { text: string; tone: string; icon?: React.ReactNode };

function StepNodeImpl({ data, selected }: NodeProps<StepFlowNode>) {
  const meta = stepStatusMeta(data.status);
  const zoom = useStore((state) => state.transform[2]);
  const density = zoom < 0.55 ? 'minimal' : zoom < 0.82 ? 'compact' : 'full';
  const kind = normalizeStepKind(data.kind);
  const hasQuestion = Boolean(data.pendingQuestion);
  const [quickLookOpen, setQuickLookOpen] = React.useState(false);

  let semanticResult: SemanticResult | undefined;
  if (kind === 'verify' && data.verifyVerdict) {
    const { pass, tally } = data.verifyVerdict;
    semanticResult = {
      text:
        pass === true
          ? `${data.verifyVerdictLabels?.pass ?? ''}${tally ? ` ${tally}` : ''}`
          : pass === false
            ? `${data.verifyVerdictLabels?.fail ?? ''}${tally ? ` ${tally}` : ''}`
            : (data.verifyVerdictLabels?.pending ?? ''),
      tone: pass === true ? 'var(--success)' : pass === false ? 'var(--danger)' : 'var(--text-secondary)',
      icon:
        pass === true ? (
          <CheckOne theme='outline' size='10' strokeWidth={4} />
        ) : pass === false ? (
          <CloseOne theme='outline' size='10' strokeWidth={4} />
        ) : undefined,
    };
  } else if (kind === 'judge' && data.judgeWinner) {
    const { winner, judges } = data.judgeWinner;
    semanticResult = {
      text:
        winner == null
          ? (data.judgeWinnerLabels?.pending ?? data.judgeWinnerLabels?.none ?? '')
          : `${data.judgeWinnerLabels?.winner ?? ''} #${winner + 1}${judges ? ` · ${judges}` : ''}`,
      tone: winner == null ? 'var(--text-secondary)' : 'var(--success)',
      icon: winner == null ? undefined : <Trophy theme='outline' size='10' strokeWidth={4} />,
    };
  } else if (kind === 'loop' && data.loopState) {
    const { state, iterations, maxIter } = data.loopState;
    semanticResult = {
      text:
        state === 'done'
          ? `${data.loopStateLabels?.done ?? ''}${iterations != null && maxIter != null ? ` ${iterations}/${maxIter}` : ''}`
          : state === 'failed'
            ? (data.loopStateLabels?.failed ?? '')
            : (data.loopStateLabels?.iterating ?? ''),
      tone: state === 'done' ? 'var(--success)' : state === 'failed' ? 'var(--danger)' : 'var(--text-secondary)',
      icon:
        state === 'done' ? (
          <CheckOne theme='outline' size='10' strokeWidth={4} />
        ) : state === 'failed' ? (
          <CloseOne theme='outline' size='10' strokeWidth={4} />
        ) : (
          <Refresh theme='outline' size='10' strokeWidth={4} />
        ),
    };
  }

  const summary = hasQuestion
    ? `${data.questionLabel ?? ''}${data.pendingQuestion ? ` · ${data.pendingQuestion}` : ''}`
    : data.errorSummary || semanticResult?.text || data.outputSummary || data.spec || data.statusLabel;

  const kindBadge =
    kind === 'synthesis'
      ? { label: data.synthesisLabel, tone: SYNTH_ACCENT, icon: <Merge theme='outline' size='11' strokeWidth={4} /> }
      : kind === 'verify'
        ? { label: data.verifyLabel, tone: VERIFY_ACCENT, icon: <Shield theme='outline' size='11' strokeWidth={4} /> }
        : kind === 'judge'
          ? { label: data.judgeLabel, tone: JUDGE_ACCENT, icon: <Gavel theme='outline' size='11' strokeWidth={4} /> }
          : kind === 'loop'
            ? { label: data.loopLabel, tone: LOOP_ACCENT, icon: <Refresh theme='outline' size='11' strokeWidth={4} /> }
            : null;

  const quickLook = (
    <div className='nomi-dag-quick-look'>
      <div className='nomi-dag-quick-look-header'>
        <span className='nomi-dag-quick-look-dot' style={{ background: meta.color }} />
        <strong>{data.title}</strong>
        <span style={{ color: meta.color }}>{data.statusLabel}</span>
      </div>
      {data.spec && <p className='nomi-dag-quick-look-spec'>{data.spec}</p>}
      {(data.outputSummary || data.errorSummary) && (
        <div className='nomi-dag-quick-look-result'>
          <span>{data.quickLookLabels.result}</span>
          <p>{data.errorSummary || data.outputSummary}</p>
        </div>
      )}
      <div className='nomi-dag-quick-look-meta'>
        {(data.roleLabel || data.chipLabel) && (
          <span>
            {data.quickLookLabels.role} · {data.roleLabel || data.chipLabel}
          </span>
        )}
        {data.modelLabel && (
          <span>
            {data.quickLookLabels.model} · {data.modelLabel}
          </span>
        )}
        {data.attempt > 1 && (
          <span>
            {data.quickLookLabels.attempts} · {data.attempt}
          </span>
        )}
        {typeof data.tokens === 'number' && data.tokens > 0 && (
          <span className='tabular-nums'>
            <Lightning theme='outline' size='11' strokeWidth={4} /> {data.tokens.toLocaleString()} {data.tokensLabel}
          </span>
        )}
      </div>
      <div className='nomi-dag-quick-look-relations'>
        <div>
          <b>{data.quickLookLabels.upstream}</b>
          <span>{data.upstreamLabels.length > 0 ? data.upstreamLabels.join(' · ') : '—'}</span>
        </div>
        <div>
          <b>{data.quickLookLabels.downstream}</b>
          <span>{data.downstreamLabels.length > 0 ? data.downstreamLabels.join(' · ') : '—'}</span>
        </div>
      </div>
      <div className='nomi-dag-quick-look-hint'>{data.quickLookLabels.inspect}</div>
    </div>
  );

  const setRelationFocus = (active: boolean) => data.onHoverChange?.(active);

  return (
    <Popover
      trigger='hover'
      position='top'
      popupVisible={quickLookOpen}
      onVisibleChange={setQuickLookOpen}
      triggerProps={{ mouseEnterDelay: 260 }}
      getPopupContainer={() => document.body}
      className='nomi-dag-quick-look-popover'
      content={quickLook}
      unmountOnExit
    >
      <div
        className='nomi-dag-node-shell nomi-dag-enter'
        onMouseEnter={() => setRelationFocus(true)}
        onMouseLeave={() => setRelationFocus(false)}
        style={{ '--dag-i': data.enterIndex ?? 0 } as React.CSSProperties}
      >
        <Handle type='target' position={Position.Top} isConnectable={false} />
        <button
          type='button'
          aria-label={`${data.title} · ${data.statusLabel} · ${data.quickLookLabels.upstream} ${data.upstreamLabels.length} · ${data.quickLookLabels.downstream} ${data.downstreamLabels.length}`}
          onClick={data.onOpen}
          onFocus={() => {
            setQuickLookOpen(true);
            setRelationFocus(true);
          }}
          onBlur={() => {
            setQuickLookOpen(false);
            setRelationFocus(false);
          }}
          className='nomi-dag-card'
          data-status={data.status}
          data-selected={selected ? 'true' : undefined}
          data-question={hasQuestion ? 'true' : undefined}
          data-relation={data.relationState ?? 'idle'}
          data-density={density}
          style={{ '--node-accent': meta.color } as React.CSSProperties}
        >
          <span
            className={`nomi-dag-status-dot ${meta.pulse ? 'nomi-dag-pulse' : ''}`}
            style={{ background: meta.color }}
          />
          <span className='nomi-dag-node-copy'>
            <span className='nomi-dag-node-title'>{data.title}</span>
            <span className='nomi-dag-node-summary' style={{ color: hasQuestion ? 'var(--warning)' : undefined }}>
              {hasQuestion && <Help theme='filled' size='10' />}
              {semanticResult?.icon && !hasQuestion && <span style={{ color: semanticResult.tone }}>{semanticResult.icon}</span>}
              <span>{summary}</span>
            </span>
          </span>
          <span className='nomi-dag-node-aside'>
            {kindBadge && (
              <span
                className='nomi-dag-kind-icon'
                title={kindBadge.label}
                style={{ color: kindBadge.tone, background: `color-mix(in srgb, ${kindBadge.tone} 11%, transparent)` }}
              >
                {kindBadge.icon}
              </span>
            )}
            {data.groupLabel && <Branch theme='outline' size='11' title={data.groupChipLabel} />}
            <span className='nomi-dag-relation-counts' aria-hidden='true'>
              <span>↑{data.upstreamLabels.length}</span>
              <span>↓{data.downstreamLabels.length}</span>
            </span>
          </span>
        </button>
        <Handle type='source' position={Position.Bottom} isConnectable={false} />
      </div>
    </Popover>
  );
}

export default React.memo(StepNodeImpl);
