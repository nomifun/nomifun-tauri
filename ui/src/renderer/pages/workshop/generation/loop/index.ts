/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Loop-node run engine (M8). The loop node card lives in
 * `canvas/nodes/LoopNode`; this sub-module owns the config types, the
 * remount-safe run registry, the round coordinator, and the React binding.
 */

export {
  DEFAULT_COUNT_TEMPLATE_ZH,
  IDLE_PROGRESS,
  LOOP_BATCH_MAX,
  LOOP_BATCH_MIN,
  LOOP_COUNT_MAX,
  LOOP_COUNT_MIN,
  LOOP_COUNT_TOKEN,
  LOOP_PARALLEL_LIMIT,
  LOOP_START_MIN,
  injectCount,
  readLoopConfig,
  type LoopConfig,
  type LoopProgress,
  type LoopRoundResult,
} from './loopTypes';
export { abortAllLoopRuns, abortLoopRun } from './loopRegistry';
export { startLoopRun } from './runLoop';
export { useLoopRunner, type LoopRunner } from './useLoopRunner';
