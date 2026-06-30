/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import type { TRunDetail } from '@/common/types/orchestrator/orchestratorTypes';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import OrchestratorComposer from '../OrchestratorComposer';

export interface RunIntentBoxProps {
  runId: string;
  /** Live run detail — read to snapshot the CURRENT task-id set for the diff. */
  detail: TRunDetail | null | undefined;
  /** Re-pull the run detail after a successful adjust (the run re-adjusts + re-drives). */
  refetch: () => Promise<void>;
  /**
   * Fired after an intent is successfully applied — surfaces the submitted intent
   * + its kept/added/removed diff so the conversation (对话) view can append it as
   * a dialogue turn. Optional: a caller that omits it gets the box's prior
   * behavior unchanged (the inline last-applied hint + the success toast).
   */
  onApplied?: (intent: string, summary: AdjustSummary) => void;
}

/** The kept / added / removed diff computed by comparing the run's task-id set
 * before the adjust against the set after the live refetch. */
interface AdjustSummary {
  kept: number;
  added: number;
  removed: number;
}

/**
 * Diff two task-id sets into a {@link AdjustSummary}:
 *  - kept    — ids present in BOTH before & after (the main agent reused the work);
 *  - added   — ids in after but not before (newly inserted tasks);
 *  - removed — ids in before but not after (dropped / re-decomposed away).
 */
function diffTaskIds(before: Set<string>, after: Set<string>): AdjustSummary {
  let kept = 0;
  let added = 0;
  for (const id of after) {
    if (before.has(id)) kept += 1;
    else added += 1;
  }
  let removed = 0;
  for (const id of before) {
    if (!after.has(id)) removed += 1;
  }
  return { kept, added, removed };
}

/**
 * RunIntentBox — the headline conversational surface of 「智能编排」: a docked
 * intent bar at the bottom of the run view where the user types, in natural
 * language, how they want the orchestration changed (「研究太浅，加一道校验和
 * 第二个研究员」). On submit it captures the run's current task-id set, calls
 * {@link ipcBridge.orchestrator.runs.adjustRun} (the one-shot main agent
 * intelligently re-adjusts the live DAG — keeping completed work that still
 * serves the intent, adding new nodes, re-decomposing others), force-refreshes
 * the run, and surfaces a transient 「保留 N · 新增 M · 移除 K」summary computed
 * by diffing the refetched task-id set against the captured one.
 *
 * The input is the shared {@link OrchestratorComposer} (chat-style rd-24 card +
 * circular send), matching the conversation page. As a docked adjust surface it
 * hides the advanced model-range / autonomy pills (those only配 a new run).
 *
 * Guards a double-submit (disabled while in-flight); an empty intent is a no-op.
 * Backend BadRequest cases (a `running` task, a cyclic plan) carry a
 * human-readable message we surface verbatim via {@link useArcoMessage}.
 */
const RunIntentBox: React.FC<RunIntentBoxProps> = ({ runId, detail, refetch, onApplied }) => {
  const { t } = useTranslation();
  const [message, msgCtx] = useArcoMessage();

  const [intent, setIntent] = useState('');
  const [submitting, setSubmitting] = useState(false);
  // The last intent that was successfully applied + its diff, shown as a subtle
  // inline hint above the input (the DAG + the toast are the primary feedback).
  const [lastApplied, setLastApplied] = useState<{ intent: string; summary: AdjustSummary } | null>(null);

  // Snapshot the CURRENT task-id set lazily at submit time (not memoized) so the
  // diff is taken against exactly what was on the canvas when the user pressed send.
  const detailRef = useRef(detail);
  detailRef.current = detail;

  const summaryLine = useMemo(() => {
    if (!lastApplied) return null;
    const { kept, added, removed } = lastApplied.summary;
    return t('orchestrator.run.intent.summary', { kept, added, removed });
  }, [lastApplied, t]);

  const handleSubmit = useCallback(
    async (value: string) => {
      if (!value || submitting) return;

      // Capture the task-id set *before* the adjust for the kept/added/removed diff.
      const before = new Set((detailRef.current?.tasks ?? []).map((task) => task.id));

      setSubmitting(true);
      try {
        await ipcBridge.orchestrator.runs.adjustRun.invoke({ run_id: runId, intent: value });
        // Fetch the authoritative post-adjust detail directly for the diff: a bare
        // `refetch()` updates `detail` via React state (not visible until the next
        // render), so we can't read the fresh task set off the ref synchronously.
        // We pull it ourselves for the diff, then `refetch()` to update the live view.
        const after = await ipcBridge.orchestrator.runs.get.invoke({ id: runId });
        const afterIds = new Set((after?.tasks ?? []).map((task) => task.id));
        const summary = diffTaskIds(before, afterIds);
        // Force the live view to re-pull (the run re-adjusts + re-drives).
        await refetch();
        setLastApplied({ intent: value, summary });
        setIntent('');
        // Surface the applied intent + diff to the conversation view (no-op in the
        // canvas view, which doesn't pass the callback).
        onApplied?.(value, summary);
        message.success(t('orchestrator.run.intent.summary', { ...summary }));
      } catch (e) {
        // The BadRequest cases are real + user-facing (a running task → 「请先暂停
        // 再重调」; a cyclic plan → 「调整计划存在循环依赖…」). The backend supplies
        // the human message; surface it verbatim, falling back to the raw error.
        const backendMsg = isBackendHttpError(e) && e.backendMessage ? e.backendMessage : '';
        message.error(
          backendMsg
            ? t('orchestrator.run.intent.error', { error: backendMsg })
            : t('orchestrator.run.intent.error', { error: String(e) })
        );
      } finally {
        setSubmitting(false);
      }
    },
    [submitting, runId, refetch, message, t, onApplied]
  );

  return (
    <div className='shrink-0 border-t border-t-base bg-1 px-16px pb-14px pt-12px'>
      {msgCtx}

      {/* Subtle 「意图历史」hint — the last intent applied + its diff. Centered to
          line up with the 800px composer column below. */}
      {lastApplied && summaryLine && (
        <div className='mx-auto mb-8px flex w-full max-w-800px items-center gap-8px overflow-hidden px-16px text-11px leading-tight text-t-tertiary'>
          <span
            className='inline-flex shrink-0 items-center rd-full px-7px py-2px text-10px font-600 tabular-nums'
            style={{
              color: 'rgb(var(--primary-6))',
              background: 'color-mix(in srgb, rgb(var(--primary-6)) 12%, transparent)',
            }}
          >
            {summaryLine}
          </span>
          <span className='min-w-0 flex-1 truncate' title={lastApplied.intent}>
            {t('orchestrator.run.intent.lastApplied', { intent: lastApplied.intent })}
          </span>
        </div>
      )}

      {/* Shared chat-style composer (docked adjust surface) — advanced pills are
          hidden here (model range / autonomy only配 a new run). */}
      <OrchestratorComposer
        value={intent}
        onChange={setIntent}
        onSubmit={handleSubmit}
        submitting={submitting}
        placeholder={t('orchestrator.run.intent.placeholder')}
        label={t('orchestrator.run.intent.label')}
      />
    </div>
  );
};

export default RunIntentBox;
