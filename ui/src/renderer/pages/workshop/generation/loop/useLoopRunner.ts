/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * React binding between a loop node and its (component-independent) run in the
 * {@link ./loopRegistry}. Subscribing here — rather than owning run state in the
 * component — means a remounted loop node re-attaches to an in-flight run and
 * keeps showing live progress.
 */

import { useCallback, useEffect, useState } from 'react';
import { useReactFlow } from '@xyflow/react';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../../canvas/model';
import type { ModelOption } from '../genTypes';
import { abortLoopRun, getLoopProgress, isLoopRunning, subscribeLoop } from './loopRegistry';
import type { LoopConfig, LoopProgress } from './loopTypes';
import { startLoopRun } from './runLoop';
import type { WorkshopNodeId } from '@/common/types/ids';

export interface UseLoopRunnerArgs {
  loopId: WorkshopNodeId;
  targetId: WorkshopNodeId | null;
  canvasId: import('@/common/types/ids').CanvasId;
  config: LoopConfig;
  model: ModelOption | null;
}

export interface LoopRunner {
  progress: LoopProgress;
  running: boolean;
  /** True when a run can be started (target wired + a model resolved). */
  canRun: boolean;
  start: () => void;
  stop: () => void;
}

export function useLoopRunner(args: UseLoopRunnerArgs): LoopRunner {
  const { loopId, targetId, canvasId, config, model } = args;
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();
  const [progress, setProgress] = useState<LoopProgress>(() => getLoopProgress(loopId));

  useEffect(() => subscribeLoop(loopId, setProgress), [loopId]);

  const running = progress.status === 'running' || isLoopRunning(loopId);
  const canRun = !!targetId && !!model && !running;

  const start = useCallback(() => {
    if (!targetId || !model) return;
    startLoopRun({
      rf,
      loopId,
      targetId,
      canvasId,
      config,
      model,
    });
  }, [rf, loopId, targetId, canvasId, config, model]);

  const stop = useCallback(() => abortLoopRun(loopId), [loopId]);

  return { progress, running, canRun, start, stop };
}
