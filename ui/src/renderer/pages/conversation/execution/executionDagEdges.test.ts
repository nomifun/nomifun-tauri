import { describe, expect, test } from 'bun:test';
import type {
  TExecutionStepDependency,
  TExecutionStepStatus,
} from '@/common/types/agentExecution/agentExecutionTypes';
import { parseExecutionStepId, type ExecutionStepId } from '@/common/types/ids';
import { buildExecutionDagEdges, EXECUTION_DAG_EDGE_STROKE } from './executionDagEdges';
import { executionDagEdgeId } from './layoutExecutionDag';

const STEP_IDS = {
  1: '0190f5fe-7c00-7a00-8000-000000000001',
  2: '0190f5fe-7c00-7a00-8000-000000000002',
  3: '0190f5fe-7c00-7a00-8000-000000000003',
  4: '0190f5fe-7c00-7a00-8000-000000000004',
} as const;

const stepId = (id: keyof typeof STEP_IDS): ExecutionStepId => parseExecutionStepId(STEP_IDS[id]);
const dependency = (source: keyof typeof STEP_IDS, target: keyof typeof STEP_IDS) =>
  ({
    blocker_step_id: stepId(source),
    blocked_step_id: stepId(target),
  }) as TExecutionStepDependency;
const statuses = (
  values: ReadonlyArray<readonly [keyof typeof STEP_IDS, TExecutionStepStatus]>,
): ReadonlyMap<ExecutionStepId, TExecutionStepStatus> =>
  new Map(values.map(([id, status]) => [stepId(id), status]));

describe('execution DAG edge projection', () => {
  test('projects every active dependency once and keeps every resting edge visible', () => {
    const dependencies = [dependency(1, 2), dependency(2, 3), dependency(3, 4)];
    const edges = buildExecutionDagEdges({
      dependencies,
      statusByStepId: statuses([
        [1, 'cancelled'],
        [2, 'failed'],
        [3, 'skipped'],
        [4, 'pending'],
      ]),
      focusedEdgeIds: null,
    });

    expect(edges).toHaveLength(dependencies.length);
    expect(edges.map((edge) => edge.id)).toEqual(
      dependencies.map((item) => executionDagEdgeId(item.blocker_step_id, item.blocked_step_id)),
    );
    for (const edge of edges) {
      expect(edge.style?.stroke).toBe(EXECUTION_DAG_EDGE_STROKE.resting);
      expect(Number(edge.style?.strokeWidth)).toBeGreaterThanOrEqual(1.75);
      expect(Number(edge.style?.opacity)).toBeGreaterThanOrEqual(0.88);
    }
  });

  test('highlights both outbound and inbound dependencies of a running step', () => {
    const outbound = buildExecutionDagEdges({
      dependencies: [dependency(1, 2)],
      statusByStepId: statuses([
        [1, 'running'],
        [2, 'pending'],
      ]),
      focusedEdgeIds: null,
    })[0];
    const inbound = buildExecutionDagEdges({
      dependencies: [dependency(1, 2)],
      statusByStepId: statuses([
        [1, 'completed'],
        [2, 'running'],
      ]),
      focusedEdgeIds: null,
    })[0];

    for (const edge of [outbound, inbound]) {
      expect(edge.animated).toBe(true);
      expect(edge.style?.stroke).toBe(EXECUTION_DAG_EDGE_STROKE.running);
      expect(Number(edge.style?.strokeWidth)).toBeGreaterThan(2);
      expect(edge.className?.includes('nomi-dag-edge-live')).toBe(true);
      expect(typeof edge.markerEnd === 'object' && edge.markerEnd?.color).toBe(EXECUTION_DAG_EDGE_STROKE.running);
    }
  });

  test('uses a stable completion tone after both dependency endpoints finish', () => {
    const edge = buildExecutionDagEdges({
      dependencies: [dependency(1, 2)],
      statusByStepId: statuses([
        [1, 'completed'],
        [2, 'completed'],
      ]),
      focusedEdgeIds: null,
    })[0];

    expect(edge.animated).toBe(false);
    expect(edge.style?.stroke).toBe(EXECUTION_DAG_EDGE_STROKE.completed);
    expect(edge.className?.includes('nomi-dag-edge-completed')).toBe(true);
    expect(typeof edge.markerEnd === 'object' && edge.markerEnd?.color).toBe(EXECUTION_DAG_EDGE_STROKE.completed);
  });

  test('gives focus visual priority without hiding dependencies outside the focused path', () => {
    const focusedId = executionDagEdgeId(stepId(1), stepId(2));
    const edges = buildExecutionDagEdges({
      dependencies: [dependency(1, 2), dependency(3, 4)],
      statusByStepId: statuses([
        [1, 'completed'],
        [2, 'completed'],
        [3, 'cancelled'],
        [4, 'cancelled'],
      ]),
      focusedEdgeIds: new Set([focusedId]),
    });
    const focused = edges[0];
    const muted = edges[1];

    expect(focused.style?.stroke).toBe(EXECUTION_DAG_EDGE_STROKE.running);
    expect(focused.style?.strokeWidth).toBe(2.75);
    expect(focused.className?.includes('nomi-dag-edge-focused')).toBe(true);
    expect(typeof focused.markerEnd === 'object' && focused.markerEnd?.color).toBe(EXECUTION_DAG_EDGE_STROKE.running);
    expect(muted.className?.includes('nomi-dag-edge-muted')).toBe(true);
    expect(muted.style?.opacity).toBe(0.88);
    expect(Number(muted.style?.strokeWidth)).toBeGreaterThanOrEqual(1.75);
  });
});
