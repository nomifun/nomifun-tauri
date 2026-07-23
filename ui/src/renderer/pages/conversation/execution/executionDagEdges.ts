import { MarkerType, type Edge } from '@xyflow/react';
import type {
  TExecutionStepDependency,
  TExecutionStepStatus,
} from '@/common/types/agentExecution/agentExecutionTypes';
import type { ExecutionStepId } from '@/common/types/ids';
import { executionDagEdgeId } from './layoutExecutionDag';

/**
 * A dependency is structural information, so its resting state must remain
 * legible independently of task status and theme border opacity.
 */
export const EXECUTION_DAG_EDGE_STROKE = {
  resting: 'color-mix(in srgb, var(--text-secondary) 74%, var(--bg-base))',
  completed: 'color-mix(in srgb, var(--success) 58%, var(--text-secondary))',
  running: 'rgb(var(--primary-6))',
  waiting: 'var(--warning)',
} as const;

const RESTING_EDGE_WIDTH = 1.75;
const ACTIVE_EDGE_WIDTH = 2.35;
const FOCUSED_EDGE_WIDTH = 2.75;
// Focus is communicated primarily by color and width. Keep the rest of the
// graph well above the former near-invisible state so structural dependencies
// remain legible while a node is selected.
const MUTED_EDGE_OPACITY = 0.88;

export interface BuildExecutionDagEdgesOptions {
  dependencies: readonly TExecutionStepDependency[];
  statusByStepId: ReadonlyMap<ExecutionStepId, TExecutionStepStatus>;
  /**
   * `null` means the canvas has no focused causal path. An empty set means a
   * focus exists but this projection contains no matching dependency.
   */
  focusedEdgeIds: ReadonlySet<string> | null;
}

/**
 * Project every active dependency to exactly one React Flow edge.
 *
 * Status only changes presentation; it never removes structural edges.
 * Focus wins over lifecycle tone, while a running node highlights both its
 * inbound and outbound dependencies.
 */
export function buildExecutionDagEdges({
  dependencies,
  statusByStepId,
  focusedEdgeIds,
}: BuildExecutionDagEdgesOptions): Edge[] {
  return dependencies.map((dependency) => {
    const id = executionDagEdgeId(dependency.blocker_step_id, dependency.blocked_step_id);
    const sourceStatus = statusByStepId.get(dependency.blocker_step_id);
    const targetStatus = statusByStepId.get(dependency.blocked_step_id);
    const pathFocused = focusedEdgeIds?.has(id) ?? false;
    const dimmed = focusedEdgeIds != null && !pathFocused;
    const running = sourceStatus === 'running' || targetStatus === 'running';
    const waiting = sourceStatus === 'waiting_input' || targetStatus === 'waiting_input';
    const completed = sourceStatus === 'completed' && targetStatus === 'completed';

    const stroke = pathFocused
      ? EXECUTION_DAG_EDGE_STROKE.running
      : running
        ? EXECUTION_DAG_EDGE_STROKE.running
        : waiting
          ? EXECUTION_DAG_EDGE_STROKE.waiting
          : completed
            ? EXECUTION_DAG_EDGE_STROKE.completed
            : EXECUTION_DAG_EDGE_STROKE.resting;
    const animated = running && !dimmed;
    const strokeWidth = pathFocused ? FOCUSED_EDGE_WIDTH : running || waiting ? ACTIVE_EDGE_WIDTH : RESTING_EDGE_WIDTH;

    return {
      id,
      source: String(dependency.blocker_step_id),
      target: String(dependency.blocked_step_id),
      type: 'smoothstep',
      animated,
      className: [
        animated ? 'nomi-dag-edge-live' : '',
        completed ? 'nomi-dag-edge-completed' : '',
        pathFocused ? 'nomi-dag-edge-focused' : '',
        dimmed ? 'nomi-dag-edge-muted' : '',
      ]
        .filter(Boolean)
        .join(' '),
      style: {
        stroke,
        strokeWidth,
        opacity: dimmed ? MUTED_EDGE_OPACITY : 1,
      },
      markerEnd: {
        type: MarkerType.ArrowClosed,
        color: stroke,
        width: 12,
        height: 12,
      },
      interactionWidth: 16,
      zIndex: pathFocused ? 2 : running || waiting ? 1 : 0,
    };
  });
}
