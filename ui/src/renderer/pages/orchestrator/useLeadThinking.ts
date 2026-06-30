/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type {
  TOrchLeadThinkingPhase,
  TOrchRunLeadThinkingEvent,
} from '@/common/types/orchestrator/orchestratorEvents';
import { useEffect, useRef, useState } from 'react';

/** Public re-exports so consumers don't reach into the wire-type module. */
export type LeadThinkingPhase = TOrchLeadThinkingPhase;
export type LeadThinkingKind = 'reasoning' | 'text' | 'phase';

/**
 * Render state for the live 编排思考 (lead-agent planning) bubble.
 *
 * `reasoning` is the accumulated reasoning text (token deltas concatenated).
 * `phaseKeys` are the semantic phase-narration keys received so far
 * (`planning_started` / `decomposing` / `assigning` …) — the frontend maps
 * these to i18n copy (Task F5). `textHeartbeat` records that draft `text`
 * deltas have arrived (used for a "拟稿中…" hint); we intentionally do NOT
 * store the draft content (it is raw plan JSON). `active` is true while the
 * stream is in flight.
 */
export interface LeadThinkingState {
  phase: LeadThinkingPhase | null;
  reasoning: string;
  phaseKeys: string[];
  active: boolean;
  textHeartbeat: boolean;
}

const EMPTY_STATE: LeadThinkingState = {
  phase: null,
  reasoning: '',
  phaseKeys: [],
  active: false,
  textHeartbeat: false,
};

/**
 * Independently subscribes to the lead-agent planning thought stream
 * (`orchestrator.run.leadThinking`) for a single run and exposes a derived
 * render state for the streaming 编排思考 bubble.
 *
 * Deliberately decoupled from {@link useRunLive}: this hook NEVER calls
 * `runs.get`, so high-frequency reasoning tokens cannot trigger run-detail
 * refetches. It filters every event by `run_id`, accumulates by `kind`
 * (reasoning → appended to `reasoning`; phase → pushed onto `phaseKeys`;
 * text → only flips `textHeartbeat`, no content stored), and stops/clears on
 * `done`, on a `planUpdated` for the same run (plan ready), and on run change /
 * unmount.
 *
 * Reasoning deltas are buffered in a ref and committed to state on a
 * `requestAnimationFrame` tick, so a burst of tokens collapses into a single
 * re-render per frame rather than one per token.
 */
export function useLeadThinking(runId: string | null): LeadThinkingState {
  const [state, setState] = useState<LeadThinkingState>(EMPTY_STATE);

  // Pending reasoning deltas awaiting the next rAF flush, plus the scheduled
  // frame handle. Kept in refs so the high-frequency event handler never
  // re-subscribes or re-renders by itself.
  const pendingReasoningRef = useRef<string>('');
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    // Reset on (re)subscription so a previous run's stream never bleeds in.
    pendingReasoningRef.current = '';
    if (rafRef.current !== null) {
      cancelAnimationFrame(rafRef.current);
      rafRef.current = null;
    }
    setState(EMPTY_STATE);

    if (!runId) return;

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

    const onLeadThinking = (e: TOrchRunLeadThinkingEvent) => {
      if (e.run_id !== runId) return;

      // `done` ends the stream: flush any buffered reasoning immediately, then
      // mark inactive. We keep the accumulated reasoning/phaseKeys so the
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
        case 'phase': {
          // `content` is the semantic phase-narration key (frontend i18n).
          const key = e.content;
          setState((prev) => {
            const phaseKeys = key && !prev.phaseKeys.includes(key) ? [...prev.phaseKeys, key] : prev.phaseKeys;
            return { ...prev, phase: e.phase, phaseKeys, active: true };
          });
          break;
        }
        case 'text': {
          // Draft plan text — record only a heartbeat, never the content.
          setState((prev) => ({ ...prev, phase: e.phase, active: true, textHeartbeat: true }));
          break;
        }
        default:
          break;
      }
    };

    // Plan ready (same run) ends the planning stream even without an explicit
    // `done` — settle to inactive.
    const onPlanUpdated = (e: { run_id: string }) => {
      if (e.run_id !== runId) return;
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

    const unsubThinking = ipcBridge.orchestrator.runEvents.leadThinking.on(onLeadThinking);
    const unsubPlan = ipcBridge.orchestrator.runEvents.planUpdated.on(onPlanUpdated);

    return () => {
      unsubThinking();
      unsubPlan();
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
      pendingReasoningRef.current = '';
    };
  }, [runId]);

  return state;
}
