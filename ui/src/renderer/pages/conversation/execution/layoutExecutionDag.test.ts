/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { TExecutionStep, TExecutionStepDependency } from '@/common/types/agentExecution/agentExecutionTypes';
import { collectExecutionDagFocus, executionDagEdgeId, layoutExecutionDag } from './layoutExecutionDag';

const step = (id: string) => ({ id } as TExecutionStep);
const dependency = (source: string, target: string) =>
  ({ blocker_step_id: source, blocked_step_id: target } as TExecutionStepDependency);

describe('execution DAG presentation model', () => {
  test('keeps compact fan-out and fan-in layers readable without overlap', () => {
    const positions = layoutExecutionDag(
      [step('root'), step('left'), step('right'), step('result')],
      [dependency('root', 'left'), dependency('root', 'right'), dependency('left', 'result'), dependency('right', 'result')],
    );

    expect(positions.root).toEqual({ x: 0, y: 0 });
    expect(positions.left.y).toBe(112);
    expect(positions.right.y).toBe(112);
    expect(Math.abs(positions.right.x - positions.left.x)).toBe(216);
    expect(positions.result).toEqual({ x: 0, y: 224 });
  });

  test('focuses only the selected step causal chain and leaves siblings muted', () => {
    const dependencies = [
      dependency('root', 'left'),
      dependency('root', 'right'),
      dependency('left', 'result'),
      dependency('right', 'result'),
    ];
    const focused = collectExecutionDagFocus('left', dependencies);

    expect([...focused.stepIds].sort()).toEqual(['left', 'result', 'root']);
    expect([...focused.edgeIds].sort()).toEqual(
      [executionDagEdgeId('left', 'result'), executionDagEdgeId('root', 'left')].sort(),
    );
    expect(focused.stepIds.has('right')).toBe(false);
  });

  test('terminates deterministically when malformed dependencies contain a cycle', () => {
    const dependencies = [dependency('a', 'b'), dependency('b', 'a')];
    const focused = collectExecutionDagFocus('a', dependencies);

    expect([...focused.stepIds].sort()).toEqual(['a', 'b']);
    expect([...focused.edgeIds].sort()).toEqual(['a->b', 'b->a']);
  });
});
