/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Loop-run registry — a module-level store keyed by loop node id.
 *
 * Why it lives outside React: a loop run spans many rounds/minutes, and the loop
 * node can unmount mid-run (the canvas uses `onlyRenderVisibleElements`, so the
 * card unmounts when panned off-screen). Keeping the {@link AbortController} and
 * live {@link LoopProgress} here — not in component state — lets a remounted node
 * re-attach to its in-flight run, and keeps progress churn out of the canvas doc
 * history / autosave. One run per loop id at a time.
 */

import { IDLE_PROGRESS, type LoopProgress } from './loopTypes';

type Listener = (p: LoopProgress) => void;

interface RunEntry {
  progress: LoopProgress;
  controller: AbortController;
  listeners: Set<Listener>;
}

const registry = new Map<string, RunEntry>();

function emit(entry: RunEntry): void {
  for (const l of entry.listeners) l(entry.progress);
}

/** Whether a run is currently in flight for this loop id. */
export function isLoopRunning(loopId: string): boolean {
  return registry.get(loopId)?.progress.status === 'running';
}

/** Current progress for a loop id (idle when there is no entry). */
export function getLoopProgress(loopId: string): LoopProgress {
  return registry.get(loopId)?.progress ?? IDLE_PROGRESS;
}

/**
 * Subscribe to a loop id's progress. Fires immediately with the current value and
 * returns an unsubscribe. Safe to call for ids with no active run.
 */
export function subscribeLoop(loopId: string, cb: Listener): () => void {
  let entry = registry.get(loopId);
  if (!entry) {
    // Park a listener bucket so an in-flight `beginLoopRun` can reuse it.
    entry = { progress: IDLE_PROGRESS, controller: new AbortController(), listeners: new Set() };
    registry.set(loopId, entry);
  }
  entry.listeners.add(cb);
  cb(entry.progress);
  return () => {
    const e = registry.get(loopId);
    if (!e) return;
    e.listeners.delete(cb);
    // Drop a fully-idle, unobserved bucket so the map doesn't leak.
    if (e.listeners.size === 0 && e.progress.status !== 'running') registry.delete(loopId);
  };
}

/**
 * Begin a run for a loop id. Returns the {@link AbortSignal} to thread through the
 * coordinator, or `null` when a run is already in flight (single-run guard).
 */
export function beginLoopRun(loopId: string, total: number): AbortSignal | null {
  const existing = registry.get(loopId);
  if (existing?.progress.status === 'running') return null;
  const controller = new AbortController();
  const entry: RunEntry = {
    controller,
    listeners: existing?.listeners ?? new Set(),
    progress: { status: 'running', total, completed: 0, success: 0, failed: [], activeRound: null },
  };
  registry.set(loopId, entry);
  emit(entry);
  return controller.signal;
}

/** Merge a progress patch and notify listeners (no-op if the run was cleared). */
export function patchLoopProgress(loopId: string, patch: Partial<LoopProgress>): void {
  const entry = registry.get(loopId);
  if (!entry) return;
  entry.progress = { ...entry.progress, ...patch };
  emit(entry);
}

/** Record one round's outcome atomically (completed/success/failed counters). */
export function recordLoopRound(loopId: string, round: number, result: 'success' | 'failed'): void {
  const entry = registry.get(loopId);
  if (!entry) return;
  const p = entry.progress;
  entry.progress = {
    ...p,
    completed: p.completed + 1,
    success: result === 'success' ? p.success + 1 : p.success,
    failed: result === 'failed' ? [...p.failed, round].sort((a, b) => a - b) : p.failed,
  };
  emit(entry);
}

/** Mark a run finished (done / canceled) while keeping its summary counters. */
export function endLoopRun(loopId: string, status: 'done' | 'canceled'): void {
  const entry = registry.get(loopId);
  if (!entry) return;
  entry.progress = { ...entry.progress, status, activeRound: null };
  emit(entry);
  if (entry.listeners.size === 0) registry.delete(loopId);
}

/** Abort an in-flight run (cancels current task + stops scheduling more rounds). */
export function abortLoopRun(loopId: string): void {
  const entry = registry.get(loopId);
  if (!entry || entry.progress.status !== 'running') return;
  entry.controller.abort();
}

/** Abort every in-flight run (called when the canvas editor unmounts). */
export function abortAllLoopRuns(): void {
  for (const entry of registry.values()) {
    if (entry.progress.status === 'running') entry.controller.abort();
  }
}
