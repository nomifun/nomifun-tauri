/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Branch, Comment, Down, Gavel, Lightning, Lock, Merge, Refresh, Robot, Shield } from '@icon-park/react';
// The decision feed is the DEFAULT (对话) view, where the DAG canvas/roster (which
// own dag-canvas.css, incl. the `nomi-dag-pulse` running-dot animation) are NOT
// mounted — import the stylesheet here so the feed's status pulse works standalone.
import './dag-canvas.css';
import type {
  TAssignment,
  TFleetMember,
  TRunDetail,
  TRunTask,
} from '@/common/types/orchestrator/orchestratorTypes';
import type { OpenTaskPayload } from './DagCanvas';
import { layoutDag } from './layoutDag';
import { memberLogo, memberShortLabel } from './memberLabel';
import { normalizeTaskKind, taskStatusMeta } from './nodes/TaskNode';

/** A single intent-exchange turn the user submitted via the docked intent box
 * THIS SESSION: their natural-language intent + the kept/added/removed diff the
 * main agent's re-adjust produced. Tracked in RunView state (light, session-only;
 * the current-decision rendering is always derivable from the live detail). */
export interface IntentTurn {
  /** Monotonic id (timestamp at apply time) — also doubles as the turn key. */
  id: number;
  /** The user's natural-language adjustment intent. */
  intent: string;
  /** The kept / added / removed diff the re-adjust produced. */
  summary: { kept: number; added: number; removed: number };
}

interface RunDecisionFeedProps {
  /** Live run detail (drives the always-present "current decision" message). */
  detail: TRunDetail;
  /** Session intent-exchange turns (lifted to RunView; newest last). */
  turns: IntentTurn[];
  /** Open a task's worker transcript — builds the same {@link OpenTaskPayload}
   * the DAG node / roster card do, so the page's existing drawer path is reused. */
  onSelectTask: (payload: OpenTaskPayload) => void;
  /** Currently-inspected task id, for the active highlight on a feed entry. */
  selectedTaskId: string | null;
  /** Re-pulls the run detail (handed into the OpenTaskPayload). */
  refetch: () => Promise<void>;
}

/** Pull the fan-out group label out of a task's `pattern_config` (a raw JSON
 * string, e.g. `{"group":"research"}`). Mirrors DagCanvas' parser so the feed
 * groups fan-out siblings the exact same way the canvas tints them. Returns the
 * trimmed label, or undefined for anything malformed (never throws). */
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

/** Deterministic hue from a group label (mirrors DagCanvas) so a fan-out cohort
 * in the feed shares the same calm tint it gets on the canvas. */
function hueForGroup(label: string): number {
  let hash = 0;
  for (let i = 0; i < label.length; i += 1) {
    hash = (hash * 31 + label.charCodeAt(i)) % 360;
  }
  return hash;
}

/** A structural "kind" descriptor for a feed entry — the badge glyph + accent
 * the structure reads by. Mirrors TaskNode's kind badges (same glyphs/tones). */
type KindMeta = { Glyph: typeof Merge; accent: string; label: string };

/** Trim an output summary to a readable inline snippet (the marker lines that
 * verify/judge/loop write are structured, so we surface the first line only). */
function outputSnippet(summary: string | null | undefined): string | null {
  if (!summary) return null;
  const firstLine = summary.split('\n').find((l) => l.trim().length > 0)?.trim();
  if (!firstLine) return null;
  return firstLine.length > 160 ? `${firstLine.slice(0, 160)}…` : firstLine;
}

/**
 * One task rendered as a readable decision row inside the main agent's
 * "编排决策" message: a kind badge, a step index, the title + role, the assigned
 * agent / model and the WHY (assignment rationale), a live status dot, and (when
 * done) a short output snippet. The whole row is a button that opens the task's
 * worker transcript via the page's existing select-task → drawer path.
 */
const DecisionTaskRow: React.FC<{
  task: TRunTask;
  index: number;
  assignment: TAssignment | null;
  member: TFleetMember | undefined;
  selected: boolean;
  kindMeta: KindMeta | null;
  groupColor: string | null;
  onOpen: () => void;
}> = ({ task, index, assignment, member, selected, kindMeta, groupColor, onOpen }) => {
  const { t } = useTranslation();
  const meta = taskStatusMeta(task.status);
  const friendly = memberShortLabel(member);
  const logo = memberLogo(member);
  const roleKey = task.role ?? member?.role_hint;
  const roleText = roleKey
    ? t(`orchestrator.run.role.${roleKey}` as 'orchestrator.run.role.planner', { defaultValue: roleKey })
    : null;
  const statusText = t(`orchestrator.run.task.status.${task.status}` as 'orchestrator.run.task.status.pending', {
    defaultValue: t('orchestrator.run.status.unknown'),
  });
  const rationale = assignment?.rationale?.trim() || t('orchestrator.run.feed.rationaleFallback');
  const snippet = task.status === 'done' || task.status === 'completed' ? outputSnippet(task.output_summary) : null;

  return (
    <div
      role='button'
      tabIndex={0}
      aria-label={`${task.title} · ${statusText}`}
      aria-pressed={selected}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          onOpen();
        }
      }}
      title={t('orchestrator.run.feed.openTask')}
      className='group/row flex cursor-pointer select-none flex-col gap-6px rd-10px px-12px py-10px outline-none transition-all duration-150'
      style={{
        background: selected ? 'color-mix(in srgb, rgb(var(--primary-6)) 8%, var(--bg-1))' : 'var(--bg-1)',
        border: `1px solid ${selected ? 'rgb(var(--primary-6))' : 'var(--border-light)'}`,
        borderLeft: `3px solid ${meta.color}`,
        boxShadow: selected ? '0 0 0 3px color-mix(in srgb, rgb(var(--primary-6)) 16%, transparent)' : undefined,
      }}
    >
      {/* Title row: step index + status dot + title + kind badge */}
      <div className='flex items-start gap-8px'>
        <span className='mt-1px shrink-0 text-11px font-700 leading-18px tabular-nums text-t-tertiary'>
          {index}
        </span>
        <span
          className={`mt-5px size-8px shrink-0 rd-full ${meta.pulse ? 'nomi-dag-pulse' : ''}`}
          style={{ background: meta.color, boxShadow: `0 0 0 2px color-mix(in srgb, ${meta.color} 20%, transparent)` }}
        />
        <span className='min-w-0 flex-1 text-13px font-600 leading-18px text-t-primary'>{task.title}</span>
        {kindMeta && (
          <span
            className='inline-flex shrink-0 items-center gap-3px rd-100px px-6px py-2px text-10px font-600 leading-none'
            style={{
              color: kindMeta.accent,
              background: `color-mix(in srgb, ${kindMeta.accent} 14%, transparent)`,
              border: `1px solid color-mix(in srgb, ${kindMeta.accent} 32%, transparent)`,
            }}
            title={kindMeta.label}
          >
            <kindMeta.Glyph theme='outline' size='10' strokeWidth={4} className='line-height-0' />
            {kindMeta.label}
          </span>
        )}
      </div>

      {/* Meta row: assigned agent · model · status · group chip */}
      <div className='flex flex-wrap items-center gap-x-8px gap-y-4px pl-22px'>
        <span
          className='inline-flex max-w-200px items-center gap-4px rd-100px px-7px py-2px text-10px leading-none text-t-secondary'
          style={{ background: 'var(--fill-0)', border: '1px solid var(--border-light)' }}
          title={member ? memberShortLabel(member) ?? member.id : undefined}
        >
          {logo ? (
            <img src={logo} alt='' className='size-11px shrink-0 object-contain' />
          ) : (
            <span className='size-5px shrink-0 rd-full' style={{ background: 'rgb(var(--primary-6))' }} />
          )}
          <span className='truncate'>{friendly ?? t('orchestrator.run.detail.assigned')}</span>
          {assignment?.locked && (
            <Lock theme='outline' size='9' strokeWidth={4} className='shrink-0 text-t-tertiary' />
          )}
        </span>
        {roleText && <span className='text-10px leading-none text-t-tertiary'>· {roleText}</span>}
        <span className='text-10px font-500 leading-none' style={{ color: meta.color }}>
          {statusText}
        </span>
        {typeof task.tokens === 'number' && task.tokens > 0 && (
          <span
            className='inline-flex shrink-0 items-center gap-3px text-10px leading-none tabular-nums text-t-tertiary'
            title={`${task.tokens.toLocaleString()} ${t('orchestrator.run.progress.tokens')}`}
          >
            <Lightning theme='outline' size='10' strokeWidth={4} className='shrink-0 line-height-0' />
            {task.tokens.toLocaleString()}
          </span>
        )}
        {groupColor && (
          <span
            className='size-7px shrink-0 rd-full'
            style={{ background: groupColor, boxShadow: `0 0 0 2px color-mix(in srgb, ${groupColor} 24%, transparent)` }}
          />
        )}
      </div>

      {/* WHY — the assignment rationale (the conversational direction the user asked for). */}
      <div className='pl-22px text-11px leading-16px text-t-secondary'>{rationale}</div>

      {/* Output snippet — only once the task settled with a summary. */}
      {snippet && (
        <div className='pl-22px text-11px leading-16px text-t-tertiary'>
          <span className='font-600'>{t('orchestrator.run.feed.outputLabel')}: </span>
          {snippet}
        </div>
      )}
    </div>
  );
};

/** A grouped band of fan-out / verify / judge / loop / synthesis tasks — a soft
 * header labels the pattern so its STRUCTURE reads in prose. */
const DecisionGroup: React.FC<{ accent: string; Glyph: typeof Merge; label: string; children: React.ReactNode }> = ({
  accent,
  Glyph,
  label,
  children,
}) => (
  <div
    className='flex flex-col gap-6px rd-12px p-8px'
    style={{ background: `color-mix(in srgb, ${accent} 6%, transparent)`, border: `1px solid color-mix(in srgb, ${accent} 22%, transparent)` }}
  >
    <div className='flex items-center gap-5px px-4px text-10px font-700 uppercase leading-none tracking-wide' style={{ color: accent }}>
      <Glyph theme='outline' size='12' strokeWidth={4} className='line-height-0' />
      <span>{label}</span>
    </div>
    {children}
  </div>
);

/**
 * RunDecisionFeed — the 对话 (decision dialogue) view of 「智能编排」: a clean,
 * chat-styled thread that makes the main agent's orchestration decisions legible
 * instead of black-box. Assembled FRONTEND-ONLY from the live {@link TRunDetail}
 * (no backend, no extra LLM):
 *
 *  • **The current orchestration decision** (always present) — a lead-agent
 *    message rendering the run's tasks as a readable "编排决策": topologically
 *    ordered (by dependency depth, then incoming order), each task a row with its
 *    kind badge, the assigned agent + model + the assignment RATIONALE, live
 *    status, and (when done) a short output snippet. Pattern cohorts (fan-out
 *    groups, verify gates, judge decisions, loops, synthesis) are wrapped in a
 *    labelled band so the STRUCTURE reads in prose. Re-renders live off refetch.
 *  • **Intent-exchange turns** — each intent the user submitted via the docked
 *    {@link RunIntentBox} THIS SESSION shows as a user-side bubble, followed by
 *    the lead agent's "已调整：保留 N · 新增 M · 移除 K" reply (the diff the box
 *    already computes). Tracked in RunView state and handed down via `turns`.
 *
 * Clicking any task row opens that task's worker transcript through the page's
 * existing select-task → drawer path (the identical {@link OpenTaskPayload} the
 * DAG nodes / roster cards build). Inherits the conversation visual language
 * (avatar + name for lead messages, right-aligned user bubbles, soft cards).
 */
const RunDecisionFeed: React.FC<RunDecisionFeedProps> = ({ detail, turns, onSelectTask, selectedTaskId, refetch }) => {
  const { t } = useTranslation();

  const assignmentByTask = useMemo(() => {
    const map = new Map<string, TAssignment>();
    for (const a of detail.assignments) map.set(a.task_id, a);
    return map;
  }, [detail.assignments]);

  const memberById = useMemo(() => {
    const map = new Map<string, TFleetMember>();
    for (const m of detail.fleet_members) map.set(m.id, m);
    return map;
  }, [detail.fleet_members]);

  // Topological order: dependency depth (longest path in), then the tasks' own
  // incoming order within a depth — the same layering layoutDag computes for the
  // canvas, so the prose reads in the same top-down order the graph flows.
  const orderedTasks = useMemo(() => {
    const tasks = detail.tasks;
    if (tasks.length === 0) return [];
    const positions = layoutDag(tasks, detail.deps);
    const indexOf = new Map(tasks.map((task, i) => [task.id, i]));
    return [...tasks].sort((a, b) => {
      const ya = positions[a.id]?.y ?? 0;
      const yb = positions[b.id]?.y ?? 0;
      if (ya !== yb) return ya - yb;
      const xa = positions[a.id]?.x ?? 0;
      const xb = positions[b.id]?.x ?? 0;
      if (xa !== xb) return xa - xb;
      return (indexOf.get(a.id) ?? 0) - (indexOf.get(b.id) ?? 0);
    });
  }, [detail.tasks, detail.deps]);

  // Per-group shared hue (deterministic from the label) so a fan-out cohort in
  // the feed gets the same calm tint it has on the canvas.
  const hueByGroup = useMemo(() => {
    const map = new Map<string, number>();
    for (const task of detail.tasks) {
      const group = parseGroupLabel(task.pattern_config);
      if (group && !map.has(group)) map.set(group, hueForGroup(group));
    }
    return map;
  }, [detail.tasks]);

  const handleOpen = useCallback(
    (task: TRunTask) => {
      onSelectTask({
        task,
        assignment: assignmentByTask.get(task.id) ?? null,
        fleetMembers: detail.fleet_members,
        runId: detail.run.id,
        refetch,
      });
    },
    [onSelectTask, assignmentByTask, detail.fleet_members, detail.run.id, refetch]
  );

  // Kind descriptor for a task's structural badge (mirrors TaskNode glyphs/tones).
  const kindMetaFor = useCallback(
    (task: TRunTask): KindMeta | null => {
      const kind = normalizeTaskKind(task.kind);
      switch (kind) {
        case 'synthesis':
          return { Glyph: Merge, accent: 'var(--brand)', label: t('orchestrator.run.kind.synthesis') };
        case 'verify':
          return { Glyph: Shield, accent: 'rgb(var(--primary-6))', label: t('orchestrator.run.kind.verify') };
        case 'judge':
          return { Glyph: Gavel, accent: 'var(--brand)', label: t('orchestrator.run.kind.judge') };
        case 'loop':
          return { Glyph: Refresh, accent: 'var(--brand)', label: t('orchestrator.run.kind.loop') };
        default:
          return null;
      }
    },
    [t]
  );

  // Render the ordered tasks, folding consecutive fan-out siblings of one group
  // into a labelled band so the parallel structure reads as one cohort. Verify /
  // judge / loop / synthesis stay inline (their kind badge already labels them),
  // but get a one-row band so the aggregator role reads when it's a lone node.
  const decisionBody = useMemo(() => {
    const blocks: React.ReactNode[] = [];
    let i = 0;
    let step = 0;
    while (i < orderedTasks.length) {
      const task = orderedTasks[i];
      const group = parseGroupLabel(task.pattern_config);
      if (group) {
        // Collect this and following tasks sharing the same fan-out group label.
        const members: TRunTask[] = [];
        while (i < orderedTasks.length && parseGroupLabel(orderedTasks[i].pattern_config) === group) {
          members.push(orderedTasks[i]);
          i += 1;
        }
        const hue = hueByGroup.get(group);
        const groupColor = hue != null ? `hsl(${hue}, 62%, 55%)` : 'rgb(var(--primary-6))';
        blocks.push(
          <DecisionGroup
            key={`group-${group}-${members[0].id}`}
            accent={groupColor}
            Glyph={Branch}
            label={t('orchestrator.run.feed.groupFanout', { label: group })}
          >
            {members.map((m) => {
              step += 1;
              const a = assignmentByTask.get(m.id) ?? null;
              return (
                <DecisionTaskRow
                  key={m.id}
                  task={m}
                  index={step}
                  assignment={a}
                  member={a ? memberById.get(a.member_id) : undefined}
                  selected={selectedTaskId === m.id}
                  kindMeta={kindMetaFor(m)}
                  groupColor={groupColor}
                  onOpen={() => handleOpen(m)}
                />
              );
            })}
          </DecisionGroup>
        );
        continue;
      }

      step += 1;
      const assignment = assignmentByTask.get(task.id) ?? null;
      const member = assignment ? memberById.get(assignment.member_id) : undefined;
      blocks.push(
        <DecisionTaskRow
          key={task.id}
          task={task}
          index={step}
          assignment={assignment}
          member={member}
          selected={selectedTaskId === task.id}
          kindMeta={kindMetaFor(task)}
          groupColor={null}
          onOpen={() => handleOpen(task)}
        />
      );
      i += 1;
    }
    return blocks;
  }, [orderedTasks, hueByGroup, assignmentByTask, memberById, selectedTaskId, kindMetaFor, handleOpen, t]);

  // ── Autoscroll to newest ────────────────────────────────────────────────────
  // Pin to the bottom when the user is already near it (so a live refetch / a new
  // turn keeps the latest in view), but don't yank them up if they scrolled away
  // to read an earlier decision. A "jump to latest" affordance re-pins on demand.
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [pinned, setPinned] = useState(true);
  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    setPinned(nearBottom);
  }, []);
  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current;
    if (el) el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' });
    setPinned(true);
  }, []);
  useEffect(() => {
    if (!pinned) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
    // Re-pin on any live change (new turn, refetched decision) while at bottom.
  }, [pinned, turns.length, detail.tasks, detail.assignments]);

  const decisionEmpty = orderedTasks.length === 0;

  return (
    <div className='relative flex size-full min-h-0 flex-col'>
      <div ref={scrollRef} onScroll={onScroll} className='min-h-0 flex-1 overflow-y-auto px-16px py-16px'>
        <div className='mx-auto flex w-full max-w-820px flex-col gap-16px'>
          {/* ── Lead-agent decision message (always present) ───────────────── */}
          <div className='flex flex-col items-start gap-6px'>
            <div className='flex items-center gap-7px'>
              <span
                className='flex size-24px shrink-0 items-center justify-center rd-8px text-white'
                style={{ background: 'rgb(var(--primary-6))' }}
              >
                <Robot theme='outline' size='15' strokeWidth={3} />
              </span>
              <span className='text-12px font-600 text-t-secondary'>{t('orchestrator.run.feed.agentName')}</span>
            </div>
            <div
              className='w-full rd-12px p-12px'
              style={{ background: 'var(--bg-2)', border: '1px solid var(--border-base)', borderRadius: '4px 12px 12px 12px' }}
            >
              {decisionEmpty ? (
                <div className='flex items-center gap-8px py-4px text-12px leading-18px text-t-tertiary'>
                  <Comment theme='outline' size='15' strokeWidth={3} className='shrink-0' />
                  <span>{t('orchestrator.run.feed.empty')}</span>
                </div>
              ) : (
                <>
                  <div className='mb-4px flex items-baseline gap-8px'>
                    <span className='text-13px font-700 text-t-primary'>{t('orchestrator.run.feed.decisionTitle')}</span>
                    <span className='text-11px text-t-tertiary'>
                      {t('orchestrator.run.feed.decisionSubtitle', { count: orderedTasks.length })}
                    </span>
                  </div>
                  <div className='mb-10px text-12px leading-18px text-t-secondary'>
                    {t('orchestrator.run.feed.decisionIntro')}
                  </div>
                  <div className='flex flex-col gap-8px'>{decisionBody}</div>
                </>
              )}
            </div>
          </div>

          {/* ── Intent-exchange turns (this session) ───────────────────────── */}
          {turns.map((turn) => (
            <div key={turn.id} className='flex flex-col gap-10px'>
              {/* User-side bubble — right-aligned, inherits the chat user bubble. */}
              <div className='flex flex-col items-end gap-4px'>
                <div
                  className='max-w-[80%] whitespace-pre-wrap break-words px-12px py-8px text-13px leading-18px'
                  style={{
                    background: 'var(--aou-2)',
                    color: 'var(--text-primary)',
                    borderRadius: '12px 4px 12px 12px',
                  }}
                >
                  {turn.intent}
                </div>
              </div>

              {/* Lead-agent diff reply — small avatar + a soft card. */}
              <div className='flex flex-col items-start gap-4px'>
                <div className='flex items-center gap-7px'>
                  <span
                    className='flex size-24px shrink-0 items-center justify-center rd-8px text-white'
                    style={{ background: 'rgb(var(--primary-6))' }}
                  >
                    <Robot theme='outline' size='15' strokeWidth={3} />
                  </span>
                  <span className='text-12px font-600 text-t-secondary'>{t('orchestrator.run.feed.agentName')}</span>
                </div>
                <div
                  className='max-w-[80%] px-12px py-8px text-13px leading-18px text-t-primary'
                  style={{ background: 'var(--bg-2)', border: '1px solid var(--border-base)', borderRadius: '4px 12px 12px 12px' }}
                >
                  {t('orchestrator.run.feed.intentReply', {
                    kept: turn.summary.kept,
                    added: turn.summary.added,
                    removed: turn.summary.removed,
                  })}
                </div>
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* Jump-to-latest — only when the user scrolled away from the bottom. */}
      {!pinned && (
        <div
          role='button'
          tabIndex={0}
          aria-label={t('orchestrator.run.feed.scrollLatest')}
          title={t('orchestrator.run.feed.scrollLatest')}
          onClick={scrollToBottom}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              scrollToBottom();
            }
          }}
          className='absolute bottom-16px right-20px flex size-32px cursor-pointer items-center justify-center rd-full text-white shadow-md transition-opacity hover:opacity-90'
          style={{ background: 'rgb(var(--primary-6))' }}
        >
          <Down theme='outline' size='16' strokeWidth={3} />
        </div>
      )}
    </div>
  );
};

export default RunDecisionFeed;
