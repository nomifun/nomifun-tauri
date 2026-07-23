/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { TExecutionStep, TExecutionStepDependency } from '@/common/types/agentExecution/agentExecutionTypes';
import { parseExecutionStepId, type ExecutionStepId } from '@/common/types/ids';
import { collectExecutionDagFocus, executionDagEdgeId, layoutExecutionDag } from './layoutExecutionDag';

const STEP_IDS = {
  1: '0190f5fe-7c00-7a00-8000-000000000001',
  2: '0190f5fe-7c00-7a00-8000-000000000002',
  3: '0190f5fe-7c00-7a00-8000-000000000003',
  4: '0190f5fe-7c00-7a00-8000-000000000004',
} as const;

const stepId = (id: keyof typeof STEP_IDS): ExecutionStepId => parseExecutionStepId(STEP_IDS[id]);
const step = (id: keyof typeof STEP_IDS) => ({ step_id: stepId(id) } as TExecutionStep);
const dependency = (source: keyof typeof STEP_IDS, target: keyof typeof STEP_IDS) =>
  ({
    blocker_step_id: stepId(source),
    blocked_step_id: stepId(target),
  }) as TExecutionStepDependency;

describe('execution DAG presentation model', () => {
  test('keeps compact fan-out and fan-in layers readable without overlap', () => {
    const positions = layoutExecutionDag(
      [step(1), step(2), step(3), step(4)],
      [dependency(1, 2), dependency(1, 3), dependency(2, 4), dependency(3, 4)],
    );

    expect(positions[stepId(1)]).toEqual({ x: 0, y: 0 });
    expect(positions[stepId(2)].y).toBe(112);
    expect(positions[stepId(3)].y).toBe(112);
    expect(Math.abs(positions[stepId(3)].x - positions[stepId(2)].x)).toBe(216);
    expect(positions[stepId(4)]).toEqual({ x: 0, y: 224 });
  });

  test('focuses only the selected step causal chain and leaves siblings muted', () => {
    const dependencies = [
      dependency(1, 2),
      dependency(1, 3),
      dependency(2, 4),
      dependency(3, 4),
    ];
    const focused = collectExecutionDagFocus(stepId(2), dependencies);

    expect([...focused.stepIds].sort()).toEqual([stepId(1), stepId(2), stepId(4)].sort());
    expect([...focused.edgeIds].sort()).toEqual(
      [executionDagEdgeId(stepId(2), stepId(4)), executionDagEdgeId(stepId(1), stepId(2))].sort(),
    );
    expect(focused.stepIds.has(stepId(3))).toBe(false);
  });

  test('terminates deterministically when malformed dependencies contain a cycle', () => {
    const dependencies = [dependency(1, 2), dependency(2, 1)];
    const focused = collectExecutionDagFocus(stepId(1), dependencies);

    expect([...focused.stepIds].sort()).toEqual([stepId(1), stepId(2)].sort());
    expect([...focused.edgeIds].sort()).toEqual(
      [executionDagEdgeId(stepId(1), stepId(2)), executionDagEdgeId(stepId(2), stepId(1))].sort(),
    );
  });
});
