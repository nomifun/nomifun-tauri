/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type {
  TAgentExecutionChangedEvent,
  TAgentExecutionLeadThinkingEvent,
  TAgentExecutionLeadThinkingPhase,
} from '@/common/types/agentExecution/agentExecutionEvents';
import type { ExecutionId } from '@/common/types/ids';
import { useEffect, useRef, useState } from 'react';

/** Public re-exports so consumers don't reach into the wire-type module. */
export type LeadThinkingPhase = TAgentExecutionLeadThinkingPhase;

/**
 * Render state for the lead participant's live planning stream.
 *
 * `reasoning` is the accumulated reasoning text (token deltas concatenated).
 * `textHeartbeat` records that draft `text`
 * deltas have arrived (used for a "拟稿中…" hint); we intentionally do NOT
 * store the draft content (it is raw plan JSON). `active` is true while the
 * stream is in flight.
 */
export interface LeadThinkingState {
  phase: LeadThinkingPhase | null;
  reasoning: string;
  active: boolean;
  textHeartbeat: boolean;
}

const EMPTY_STATE: LeadThinkingState = {
  phase: null,
  reasoning: '',
  active: false,
  textHeartbeat: false,
};

/**
 * Independently subscribes to the lead participant's planning stream for one
 * execution and exposes a render-friendly state.
 *
 * Deliberately decoupled from {@link useExecutionLive}: this hook never loads
 * execution detail, so high-frequency reasoning tokens cannot trigger detail
 * refetches. It filters every event by `execution_id`, accumulates by `kind`
 * (reasoning → appended to `reasoning`; text → only flips `textHeartbeat`,
 * no content stored), and stops/clears on
 * `done`, on a plan change for the same execution, and on execution change or
 * unmount.
 *
 * Reasoning deltas are buffered in a ref and committed to state on a
 * `requestAnimationFrame` tick, so a burst of tokens collapses into a single
 * re-render per frame rather than one per token.
 */
export function useLeadThinking(executionId: ExecutionId | null): LeadThinkingState {
  const [state, setState] = useState<LeadThinkingState>(EMPTY_STATE);

  // Pending reasoning deltas awaiting the next rAF flush, plus the scheduled
  // frame handle. Kept in refs so the high-frequency event handler never
  // re-subscribes or re-renders by itself.
  const pendingReasoningRef = useRef<string>('');
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    // Reset on subscription so a previous execution's stream never bleeds in.
    pendingReasoningRef.current = '';
    if (rafRef.current !== null) {
      cancelAnimationFrame(rafRef.current);
      rafRef.current = null;
    }
    setState(EMPTY_STATE);

    if (!executionId) return;

    const flushReasoning = () => {
      rafRef.current = null;
      const pending = pendingReasoningRef.current;
      if (!pending) return;
      pendingReasoningRef.current = '';
      setState((prev) => ({ ...prev, reasoning: prev.reasoning + pending }));
    };

    const scheduleFlush = () => {
      if (rafRef.current !== null) return;
      rafRef.current = requestAnimationFrame(flushReasoning);
    };

    const onLeadThinking = (e: TAgentExecutionLeadThinkingEvent) => {
      if (e.execution_id !== executionId) return;

      // `done` ends the stream: flush any buffered reasoning immediately, then
      // mark inactive. We keep the accumulated reasoning so the
      // bubble can collapse into a summary rather than blanking out.
      if (e.done) {
        if (rafRef.current !== null) {
          cancelAnimationFrame(rafRef.current);
          rafRef.current = null;
        }
        const pending = pendingReasoningRef.current;
        pendingReasoningRef.current = '';
        setState((prev) => ({
          ...prev,
          phase: e.phase,
          reasoning: pending ? prev.reasoning + pending : prev.reasoning,
          active: false,
        }));
        return;
      }

      switch (e.kind) {
        case 'reasoning': {
          // Buffer the token; commit on the next animation frame.
          if (e.delta) pendingReasoningRef.current += e.delta;
          setState((prev) => (prev.active && prev.phase === e.phase ? prev : { ...prev, phase: e.phase, active: true }));
          scheduleFlush();
          break;
        }
        case 'text': {
          // Draft plan text — record only a heartbeat, never the content.
          setState((prev) => ({
            ...prev,
            phase: e.phase,
            active: true,
            textHeartbeat: true,
          }));
          break;
        }
        default:
          break;
      }
    };

    // A ready plan ends the planning stream even without an explicit
    // `done` — settle to inactive.
    const onExecutionChanged = (e: TAgentExecutionChangedEvent) => {
      if (e.execution_id !== executionId || e.change_kind !== 'plan_changed') return;
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
      const pending = pendingReasoningRef.current;
      pendingReasoningRef.current = '';
      setState((prev) => ({
        ...prev,
        reasoning: pending ? prev.reasoning + pending : prev.reasoning,
        active: false,
      }));
    };

    const unsubThinking = ipcBridge.agentExecution.events.leadThinking.on(onLeadThinking);
    const unsubPlan = ipcBridge.agentExecution.events.changed.on(onExecutionChanged);

    return () => {
      unsubThinking();
      unsubPlan();
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
      pendingReasoningRef.current = '';
    };
  }, [executionId]);

  return state;
}
