/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Loop-node runtime types + config readers (M8).
 *
 * The loop node persists only its **configuration** (`WorkshopLoopNodeData`);
 * live run progress lives in an in-memory registry ({@link ./loopRegistry}) so a
 * multi-round run survives the node unmounting (react-flow's
 * `onlyRenderVisibleElements` unmounts off-screen nodes) without spamming history
 * or the autosave with per-round writes.
 */

import type { WorkshopLoopMode, WorkshopLoopNodeData } from '../../types';

export const LOOP_COUNT_MIN = 1;
export const LOOP_COUNT_MAX = 50;
export const LOOP_START_MIN = 1;
export const LOOP_BATCH_MIN = 1;
export const LOOP_BATCH_MAX = 20;

/** Rolling-window size for the parallel dispatch mode. */
export const LOOP_PARALLEL_LIMIT = 3;

/** Poll cadence for a round's task (ms) — matches the generator card's engine. */
export const LOOP_POLL_INTERVAL_MS = 2500;

/** The `{i}` placeholder callers can drop into a count-injection template. */
export const LOOP_COUNT_TOKEN = '{i}';

/** Default count-injection templates by locale-ish intent (fallbacks). */
export const DEFAULT_COUNT_TEMPLATE_ZH = '现在生成第 {i} 张';

export interface LoopConfig {
  count: number;
  start: number;
  batch: number;
  loopMode: WorkshopLoopMode;
  countTemplate: string;
}

/** Terminal state of a single loop round. */
export type LoopRoundResult = 'success' | 'failed' | 'canceled';

/** Live progress of a loop run (registry-owned, transient). */
export interface LoopProgress {
  status: 'idle' | 'running' | 'done' | 'canceled';
  /** Total rounds the run was started with. */
  total: number;
  /** Rounds that have reached a terminal state. */
  completed: number;
  /** Count of succeeded rounds. */
  success: number;
  /** 1-based round numbers that failed. */
  failed: number[];
  /** The round currently in flight (for the injected-text preview / animation). */
  activeRound: number | null;
}

export const IDLE_PROGRESS: LoopProgress = {
  status: 'idle',
  total: 0,
  completed: 0,
  success: 0,
  failed: [],
  activeRound: null,
};

function clampInt(value: unknown, min: number, max: number, fallback: number): number {
  const n = typeof value === 'number' && Number.isFinite(value) ? Math.round(value) : fallback;
  return Math.min(max, Math.max(min, n));
}

/** Read a loop node's `data` into a validated {@link LoopConfig}. */
export function readLoopConfig(data: Partial<WorkshopLoopNodeData> | undefined): LoopConfig {
  return {
    count: clampInt(data?.count, LOOP_COUNT_MIN, LOOP_COUNT_MAX, 4),
    start: clampInt(data?.start, LOOP_START_MIN, Number.MAX_SAFE_INTEGER, 1),
    batch: clampInt(data?.batch, LOOP_BATCH_MIN, LOOP_BATCH_MAX, 1),
    loopMode: data?.loopMode === 'parallel' ? 'parallel' : 'serial',
    countTemplate: typeof data?.countTemplate === 'string' ? data.countTemplate : DEFAULT_COUNT_TEMPLATE_ZH,
  };
}

/** Render a count-injection template for a given 1-based round number. */
export function injectCount(template: string, round: number): string {
  const line = template.replace(/\{i\}/g, String(round)).trim();
  return line;
}
