/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TExecutionStep, TExecutionStepDependency } from '@/common/types/agentExecution/agentExecutionTypes';

/** Horizontal step between sibling tasks within the same dependency layer. */
const COL_STEP = 216;
/** Vertical step between dependency layers (depth → row). */
const ROW_STEP = 112;

export function executionDagEdgeId(source: string, target: string): string {
  return `${source}->${target}`;
}

export interface ExecutionDagFocus {
  stepIds: Set<string>;
  edgeIds: Set<string>;
}

/**
 * Collect every ancestor and descendant of one step so the canvas can reveal
 * its causal path without permanently adding labels or extra chrome.
 */
export function collectExecutionDagFocus(
  stepId: string,
  dependencies: TExecutionStepDependency[],
): ExecutionDagFocus {
  const inbound = new Map<string, TExecutionStepDependency[]>();
  const outbound = new Map<string, TExecutionStepDependency[]>();
  for (const dependency of dependencies) {
    const incoming = inbound.get(dependency.blocked_step_id);
    if (incoming) incoming.push(dependency);
    else inbound.set(dependency.blocked_step_id, [dependency]);

    const outgoing = outbound.get(dependency.blocker_step_id);
    if (outgoing) outgoing.push(dependency);
    else outbound.set(dependency.blocker_step_id, [dependency]);
  }

  const stepIds = new Set<string>([stepId]);
  const edgeIds = new Set<string>();
  const walk = (
    adjacency: Map<string, TExecutionStepDependency[]>,
    nextStep: (dependency: TExecutionStepDependency) => string,
  ) => {
    const visited = new Set<string>([stepId]);
    const queue = [stepId];
    while (queue.length > 0) {
      const current = queue.shift();
      if (!current) continue;
      for (const dependency of adjacency.get(current) ?? []) {
        edgeIds.add(executionDagEdgeId(dependency.blocker_step_id, dependency.blocked_step_id));
        const next = nextStep(dependency);
        stepIds.add(next);
        if (!visited.has(next)) {
          visited.add(next);
          queue.push(next);
        }
      }
    }
  };

  walk(inbound, (dependency) => dependency.blocker_step_id);
  walk(outbound, (dependency) => dependency.blocked_step_id);
  return { stepIds, edgeIds };
}

/**
 * Compute simple topological-layered positions for an execution task DAG.
 *
 * Each task's vertical layer is its **depth** = the longest path of `blocker →
 * blocked` edges leading into it (roots are depth 0). Tasks sharing a depth are
 * spread horizontally so siblings never overlap. The result is a pure
 * `taskId → {x,y}` map; callers prefer `task.graph_x/graph_y` when present and
 * fall back to this layout otherwise.
 *
 * The depth pass is a fixpoint relaxation capped at `tasks.length` iterations,
 * so a malformed plan with a dependency **cycle** can never spin forever — it
 * simply settles at whatever depths the cap allows (good enough for a render).
 */
export function layoutExecutionDag(
  steps: TExecutionStep[],
  dependencies: TExecutionStepDependency[],
): Record<string, { x: number; y: number }> {
  const positions: Record<string, { x: number; y: number }> = {};
  if (steps.length === 0) return positions;

  // Only consider dependencies whose endpoints are current tasks.
  const stepIds = new Set(steps.map((step) => step.id));
  const edges = dependencies.filter((dependency) => stepIds.has(dependency.blocker_step_id) && stepIds.has(dependency.blocked_step_id));

  // depth[id] = longest path (in edges) from any root to this task.
  const depth = new Map<string, number>();
  for (const step of steps) depth.set(step.id, 0);

  // Relax depths to a fixpoint: a task sits one layer below its deepest blocker.
  // Cap iterations at task count so a cycle can't loop forever.
  for (let iter = 0; iter < steps.length; iter++) {
    let changed = false;
    for (const e of edges) {
      const next = (depth.get(e.blocker_step_id) ?? 0) + 1;
      if (next > (depth.get(e.blocked_step_id) ?? 0)) {
        depth.set(e.blocked_step_id, next);
        changed = true;
      }
    }
    if (!changed) break;
  }

  // Bucket tasks by depth, preserving their incoming order for stable columns.
  const layers = new Map<number, string[]>();
  for (const step of steps) {
    const d = depth.get(step.id) ?? 0;
    const bucket = layers.get(d);
    if (bucket) bucket.push(step.id);
    else layers.set(d, [step.id]);
  }

  // Order each layer by the average position of its already-placed blockers.
  // This small barycentric pass removes most avoidable fan-in/fan-out crossings
  // while retaining the planner's original order as a deterministic fallback.
  const originalOrder = new Map<string, number>(steps.map((step, index) => [step.id, index]));
  const blockerIds = new Map<string, string[]>();
  for (const edge of edges) {
    const blockers = blockerIds.get(edge.blocked_step_id);
    if (blockers) blockers.push(edge.blocker_step_id);
    else blockerIds.set(edge.blocked_step_id, [edge.blocker_step_id]);
  }
  const placedOrder = new Map<string, number>();

  // Place each layer as a centered horizontal row so the graph reads top-down.
  for (const [d, ids] of [...layers.entries()].sort(([left], [right]) => left - right)) {
    ids.sort((left, right) => {
      const barycenter = (id: string) => {
        const upstream = (blockerIds.get(id) ?? [])
          .map((blockerId) => placedOrder.get(blockerId))
          .filter((value): value is number => value != null);
        return upstream.length > 0
          ? upstream.reduce((sum, value) => sum + value, 0) / upstream.length
          : (originalOrder.get(id) ?? 0);
      };
      return barycenter(left) - barycenter(right) || (originalOrder.get(left) ?? 0) - (originalOrder.get(right) ?? 0);
    });
    const rowWidth = (ids.length - 1) * COL_STEP;
    const startX = -rowWidth / 2;
    ids.forEach((id, col) => {
      positions[id] = { x: startX + col * COL_STEP, y: d * ROW_STEP };
      placedOrder.set(id, col);
    });
  }

  return positions;
}
