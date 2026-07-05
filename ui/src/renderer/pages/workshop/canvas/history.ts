/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Snapshot-based undo / redo for the canvas.
 *
 * Contract (M1 spec §7): shallow snapshots of `nodes + edges + background`
 * (NOT viewport); a 180 ms quiet window coalesces rapid edits into one entry;
 * node drags are suspended and land exactly one entry on release; history is
 * capped at 50 steps.
 *
 * The hook is intentionally imperative: the editor mirrors its live state into
 * refs and calls {@link CanvasHistory.record} after each mutation. `record`
 * compares content signatures, so pure selection / measurement churn (which the
 * snapshot strips) never creates spurious entries.
 */

import { useEffect, useMemo, useRef, useState } from 'react';
import type { CanvasSnapshot } from './model';
import { snapshotSignature } from './model';

const COALESCE_MS = 180;
const MAX_STEPS = 50;

export interface CanvasHistory {
  /** Coalesced record of the current state (call after any content mutation). */
  record: () => void;
  /** Record a discrete step immediately (used on drag / resize end). */
  commitNow: () => void;
  /** Snapshot the pre-interaction baseline before a drag / resize begins. */
  beginInteraction: () => void;
  undo: () => CanvasSnapshot | null;
  redo: () => CanvasSnapshot | null;
  canUndo: boolean;
  canRedo: boolean;
  /** Reset history to a fresh baseline (e.g. after (re)loading a doc). */
  reset: (baseline: CanvasSnapshot) => void;
}

/**
 * @param getSnapshot reads the current content-only snapshot from live state.
 */
export function useCanvasHistory(getSnapshot: () => CanvasSnapshot): CanvasHistory {
  const getRef = useRef(getSnapshot);
  getRef.current = getSnapshot;

  const pastRef = useRef<CanvasSnapshot[]>([]);
  const futureRef = useRef<CanvasSnapshot[]>([]);
  const baselineRef = useRef<CanvasSnapshot | null>(null);
  const baselineSigRef = useRef<string>('');
  const burstTimerRef = useRef<number | null>(null);
  const interactionBaselineRef = useRef<CanvasSnapshot | null>(null);

  const [counts, setCounts] = useState({ past: 0, future: 0 });
  const sync = (): void => setCounts({ past: pastRef.current.length, future: futureRef.current.length });

  useEffect(() => {
    return () => {
      if (burstTimerRef.current != null) window.clearTimeout(burstTimerRef.current);
    };
  }, []);

  const pushPast = (snap: CanvasSnapshot): void => {
    pastRef.current.push(snap);
    if (pastRef.current.length > MAX_STEPS) pastRef.current.shift();
    futureRef.current = [];
  };

  return useMemo<CanvasHistory>(() => {
    const applyBurst = (): void => {
      burstTimerRef.current = null;
      const next = getRef.current();
      const nextSig = snapshotSignature(next);
      if (nextSig === baselineSigRef.current) return; // nothing meaningful changed
      if (baselineRef.current) pushPast(baselineRef.current);
      baselineRef.current = next;
      baselineSigRef.current = nextSig;
      sync();
    };

    return {
      record: () => {
        if (burstTimerRef.current != null) window.clearTimeout(burstTimerRef.current);
        burstTimerRef.current = window.setTimeout(applyBurst, COALESCE_MS);
      },
      commitNow: () => {
        if (burstTimerRef.current != null) {
          window.clearTimeout(burstTimerRef.current);
          burstTimerRef.current = null;
        }
        const next = getRef.current();
        const nextSig = snapshotSignature(next);
        const anchor = interactionBaselineRef.current ?? baselineRef.current;
        interactionBaselineRef.current = null;
        if (nextSig === baselineSigRef.current) return;
        if (anchor) pushPast(anchor);
        baselineRef.current = next;
        baselineSigRef.current = nextSig;
        sync();
      },
      beginInteraction: () => {
        // Flush any pending burst so the baseline reflects pre-interaction state,
        // then capture that baseline for a single post-interaction entry.
        if (burstTimerRef.current != null) {
          window.clearTimeout(burstTimerRef.current);
          burstTimerRef.current = null;
        }
        interactionBaselineRef.current = baselineRef.current ?? getRef.current();
      },
      undo: () => {
        if (burstTimerRef.current != null) {
          window.clearTimeout(burstTimerRef.current);
          burstTimerRef.current = null;
        }
        if (pastRef.current.length === 0) return null;
        const current = getRef.current();
        futureRef.current.push(current);
        const snap = pastRef.current.pop() as CanvasSnapshot;
        baselineRef.current = snap;
        baselineSigRef.current = snapshotSignature(snap);
        sync();
        return snap;
      },
      redo: () => {
        if (burstTimerRef.current != null) {
          window.clearTimeout(burstTimerRef.current);
          burstTimerRef.current = null;
        }
        if (futureRef.current.length === 0) return null;
        const current = getRef.current();
        pastRef.current.push(current);
        const snap = futureRef.current.pop() as CanvasSnapshot;
        baselineRef.current = snap;
        baselineSigRef.current = snapshotSignature(snap);
        sync();
        return snap;
      },
      canUndo: counts.past > 0,
      canRedo: counts.future > 0,
      reset: (baseline: CanvasSnapshot) => {
        if (burstTimerRef.current != null) {
          window.clearTimeout(burstTimerRef.current);
          burstTimerRef.current = null;
        }
        pastRef.current = [];
        futureRef.current = [];
        interactionBaselineRef.current = null;
        baselineRef.current = baseline;
        baselineSigRef.current = snapshotSignature(baseline);
        sync();
      },
    };
    // `counts` drives canUndo/canRedo; the imperative methods read refs so are stable.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [counts]);
}
