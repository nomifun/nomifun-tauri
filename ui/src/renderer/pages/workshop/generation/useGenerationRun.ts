/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * The generation card's run engine: build the request, submit it, poll to a
 * terminal state, and reflect every transition back into node `data` (through
 * the canvas `updateNodeData`, so history + autosave stay consistent). Polling
 * survives remounts, while saved snapshots are revalidated against their
 * authoritative task before retaining success. Polling is torn down on
 * unmount / canvas close.
 */

import { useCallback, useEffect, useRef } from 'react';
import { useReactFlow } from '@xyflow/react';
import { cancelTask, createTask, getTask } from '../api';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import type {
  CreationTask,
  CreationTaskStatus,
  WorkshopGeneratorMode,
  WorkshopGeneratorNodeData,
  WorkshopGeneratorStatus,
} from '../types';
import { buildTaskParams } from './genConstants';
import type { ModelOption } from './genTypes';
import { buildRunPlan } from './pipeline';
import { spawnResultNodes } from './spawn';
import { EMPTY_GENERATION_ARTIFACTS_ERROR, succeededArtifactIds } from './taskArtifacts';
import type { CreationTaskId, WorkshopNodeId } from '@/common/types/ids';

const POLL_INTERVAL_MS = 2000;

const TERMINAL: CreationTaskStatus[] = ['succeeded', 'failed', 'canceled'];
const isTerminal = (s: CreationTaskStatus): boolean => TERMINAL.includes(s);

export const GENERATION_TASK_REQUIRED_ERROR =
  'Saved generation result has no authoritative task. Run the generation again.';
export const GENERATION_TASK_VERIFICATION_ERROR =
  'The saved generation task could not be verified. Retrying in the background.';
export const GENERATION_TASK_SCOPE_ERROR = 'The saved generation task does not belong to this generator card.';
export const UNSUPPORTED_GENERATION_RESULT_ERROR = 'The generation task returned an unsupported artifact type.';

/** The persisted task capability, not a mutable tab snapshot, owns the result kind. */
export function generationModeForTask(task: Pick<CreationTask, 'capability'>): WorkshopGeneratorMode | null {
  switch (task.capability) {
    case 'text':
      return 'text';
    case 't2v':
    case 'i2v':
    case 'v2v':
      return 'video';
    case 't2i':
    case 'i2i':
    case 'inpaint':
      return 'image';
    case 'tts':
    default:
      return null;
  }
}

export type MountedGenerationAuditResult =
  | { kind: 'none' }
  | { kind: 'task'; task: CreationTask }
  | { kind: 'task-unavailable' }
  | { kind: 'task-missing' };

/**
 * Reconcile every saved task id, including terminal snapshots. A successful
 * card without a task id is invalid in v3 and is never reconstructed from
 * result asset ids.
 */
export async function auditMountedGenerationSnapshot(
  snapshot: Pick<WorkshopGeneratorNodeData, 'status' | 'taskId'>,
  dependencies: {
    fetchTask?: (taskId: CreationTaskId) => Promise<CreationTask>;
  } = {}
): Promise<MountedGenerationAuditResult> {
  if (snapshot.taskId) {
    try {
      return { kind: 'task', task: await (dependencies.fetchTask ?? getTask)(snapshot.taskId) };
    } catch {
      return { kind: 'task-unavailable' };
    }
  }
  if (snapshot.status !== 'success') return { kind: 'none' };
  return { kind: 'task-missing' };
}

function mapStatus(s: CreationTaskStatus): WorkshopGeneratorStatus {
  switch (s) {
    case 'queued':
      return 'queued';
    case 'running':
      return 'running';
    case 'succeeded':
      return 'success';
    case 'failed':
      return 'error';
    case 'canceled':
    default:
      return 'idle';
  }
}

/**
 * A task already saved as terminal may already have fanned out its batch. Only
 * snapshots that were genuinely in flight may fan out after mount-time polling.
 */
export function allowSpawnAfterMountedSnapshot(status: WorkshopGeneratorStatus): boolean {
  return status === 'queued' || status === 'running';
}

export interface TerminalGenerationResolution {
  patch: Partial<WorkshopGeneratorNodeData>;
  resultMode: WorkshopGeneratorMode | null;
  resultAssetIds: import('@/common/types/ids').AssetId[];
}

/** Pure terminal-state reducer shared by live completion and reopen audits. */
export function resolveTerminalGenerationTask(task: CreationTask): TerminalGenerationResolution {
  if (task.status === 'succeeded') {
    const results = succeededArtifactIds(task);
    if (!results) {
      return {
        patch: {
          status: 'error',
          taskId: task.creation_task_id,
          resultAssetIds: [],
          errorMessage: EMPTY_GENERATION_ARTIFACTS_ERROR,
          batch: undefined,
        },
        resultMode: null,
        resultAssetIds: [],
      };
    }
    const resultMode = generationModeForTask(task);
    if (!resultMode) {
      return {
        patch: {
          status: 'error',
          taskId: task.creation_task_id,
          resultAssetIds: [],
          errorMessage: UNSUPPORTED_GENERATION_RESULT_ERROR,
          batch: undefined,
        },
        resultMode: null,
        resultAssetIds: [],
      };
    }
    return {
      patch: {
        status: 'success',
        taskId: task.creation_task_id,
        mode: resultMode,
        resultAssetIds: results,
        errorMessage: undefined,
        batch: results.length > 1 ? { expanded: true, primary: results[0] } : undefined,
      },
      resultMode,
      resultAssetIds: results,
    };
  }
  if (task.status === 'failed') {
    return {
      patch: {
        status: 'error',
        taskId: task.creation_task_id,
        resultAssetIds: [],
        errorMessage: task.error?.message || 'error',
        batch: undefined,
      },
      resultMode: null,
      resultAssetIds: [],
    };
  }
  return {
    patch: { status: 'idle', taskId: null, resultAssetIds: [], errorMessage: undefined, batch: undefined },
    resultMode: null,
    resultAssetIds: [],
  };
}

export interface UseGenerationRunArgs {
  nodeId: WorkshopNodeId;
  canvasId: import('@/common/types/ids').CanvasId;
  data: WorkshopGeneratorNodeData;
  /** The model a run should use (explicit selection, else first available). */
  effectiveModel: ModelOption | null;
  updateNodeData: (nodeId: WorkshopNodeId, patch: Partial<WorkshopGeneratorNodeData>) => void;
}

export interface GenerationRun {
  run: () => void;
  cancel: () => void;
}

export function useGenerationRun(args: UseGenerationRunArgs): GenerationRun {
  const { nodeId, canvasId } = args;
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();

  // Latest-value refs so the imperative loop never reads stale props.
  const dataRef = useRef(args.data);
  dataRef.current = args.data;
  const modelRef = useRef(args.effectiveModel);
  modelRef.current = args.effectiveModel;
  const updateRef = useRef(args.updateNodeData);
  updateRef.current = args.updateNodeData;

  const mountedRef = useRef(true);
  const timerRef = useRef<number | null>(null);
  const activeTaskRef = useRef<CreationTaskId | null>(null);
  const spawnedTaskRef = useRef<CreationTaskId | null>(null);
  const operationRef = useRef(0);

  const patch = useCallback((p: Partial<WorkshopGeneratorNodeData>) => updateRef.current(nodeId, p), [nodeId]);

  const clearTimer = useCallback(() => {
    if (timerRef.current != null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  }, []);

  const taskBelongsToCard = useCallback(
    (task: CreationTask) => task.node_id === nodeId && task.canvas_id === canvasId,
    [canvasId, nodeId]
  );

  const rejectOutOfScopeTask = useCallback(
    (taskId: CreationTaskId) => {
      activeTaskRef.current = null;
      clearTimer();
      patch({
        status: 'error',
        taskId,
        resultAssetIds: [],
        errorMessage: GENERATION_TASK_SCOPE_ERROR,
        batch: undefined,
      });
    },
    [clearTimer, patch]
  );

  const finalize = useCallback(
    (task: CreationTask, options: { allowSpawn?: boolean } = {}) => {
      activeTaskRef.current = null;
      clearTimer();
      const resolution = resolveTerminalGenerationTask(task);
      patch(resolution.patch);
      if (resolution.resultMode) {
        // Only a live completion fans out nodes. A mount/reopen audit calls
        // finalize with allowSpawn=false so persisted canvases never duplicate
        // the same batch on every reopen; every result is still visible in-card.
        if (
          options.allowSpawn !== false &&
          resolution.resultAssetIds.length > 1 &&
          spawnedTaskRef.current !== task.creation_task_id
        ) {
          spawnedTaskRef.current = task.creation_task_id;
          const card = rf.getNode(nodeId);
          if (card) {
            const operation = operationRef.current;
            void spawnResultNodes(rf, card, resolution.resultMode, resolution.resultAssetIds.slice(1), {
              shouldCommit: () => mountedRef.current && operationRef.current === operation,
            }).catch(() => {});
          }
        }
      }
    },
    [clearTimer, patch, rf, nodeId]
  );

  const poll = useCallback(
    async (taskId: CreationTaskId, allowSpawn: boolean) => {
      let task: CreationTask;
      try {
        task = await getTask(taskId);
      } catch {
        // Transient fetch error — retry on the next tick if still the active task.
        if (mountedRef.current && activeTaskRef.current === taskId) {
          timerRef.current = window.setTimeout(() => void poll(taskId, allowSpawn), POLL_INTERVAL_MS);
        }
        return;
      }
      if (!mountedRef.current || activeTaskRef.current !== taskId) return;
      if (task.creation_task_id !== taskId || !taskBelongsToCard(task)) {
        rejectOutOfScopeTask(taskId);
        return;
      }
      if (isTerminal(task.status)) {
        finalize(task, { allowSpawn });
        return;
      }
      patch({ status: mapStatus(task.status) });
      timerRef.current = window.setTimeout(() => void poll(taskId, allowSpawn), POLL_INTERVAL_MS);
    },
    [finalize, patch, rejectOutOfScopeTask, taskBelongsToCard]
  );

  const startPolling = useCallback(
    (taskId: CreationTaskId, allowSpawn: boolean) => {
      activeTaskRef.current = taskId;
      clearTimer();
      timerRef.current = window.setTimeout(() => void poll(taskId, allowSpawn), POLL_INTERVAL_MS);
    },
    [clearTimer, poll]
  );

  const run = useCallback(async () => {
    const d = dataRef.current;
    const model = modelRef.current;
    if (!model) return;
    if (d.status === 'queued' || d.status === 'running') return;

    const operation = operationRef.current + 1;
    operationRef.current = operation;
    clearTimer();
    activeTaskRef.current = null;

    const nodes = rf.getNodes();
    const edges = rf.getEdges();
    const self = nodes.find((n) => n.id === nodeId);
    if (!self) return;

    let plan;
    try {
      plan = await buildRunPlan({
        node: self,
        nodes,
        edges,
        mode: d.mode,
        mentions: d.mentions ?? [],
        maskAssetId: d.maskAssetId,
        basePrompt: d.prompt ?? '',
      });
    } catch (e) {
      patch({ status: 'error', errorMessage: e instanceof Error ? e.message : String(e) });
      return;
    }
    if (!mountedRef.current || operationRef.current !== operation) return;

    const params = buildTaskParams(d.mode, d.params ?? {}, plan.prompt);
    patch({
      status: 'queued',
      providerId: model.providerId,
      model: model.model,
      errorMessage: undefined,
      resultAssetIds: [],
      batch: undefined,
    });

    try {
      const task = await createTask({
        canvas_id: canvasId,
        node_id: nodeId,
        provider_id: model.providerId,
        model: model.model,
        capability: plan.capability,
        params,
        inputs: plan.inputs,
      });
      if (!mountedRef.current || operationRef.current !== operation) return;
      if (!taskBelongsToCard(task)) {
        rejectOutOfScopeTask(task.creation_task_id);
        return;
      }
      patch({ taskId: task.creation_task_id, status: mapStatus(task.status) });
      if (isTerminal(task.status)) finalize(task);
      else startPolling(task.creation_task_id, true);
    } catch (e) {
      if (mountedRef.current && operationRef.current === operation) {
        patch({ status: 'error', errorMessage: e instanceof Error ? e.message : String(e) });
      }
    }
  }, [rf, nodeId, canvasId, patch, finalize, startPolling, clearTimer, rejectOutOfScopeTask, taskBelongsToCard]);

  const cancel = useCallback(() => {
    const taskId = activeTaskRef.current ?? dataRef.current.taskId ?? null;
    operationRef.current += 1;
    clearTimer();
    activeTaskRef.current = null;
    patch({ status: 'idle', taskId: null });
    if (taskId) void cancelTask(taskId).catch(() => {});
  }, [clearTimer, patch]);

  // Reconcile every saved task (including terminal snapshots) after a remount /
  // canvas reopen. A successful snapshot without a task is rejected outright.
  // Mount audits never fan out nodes; live transitions own that side effect.
  useEffect(() => {
    mountedRef.current = true;
    const d = dataRef.current;
    const needsAudit = !!d.taskId || d.status === 'success';
    if (needsAudit) {
      const operation = operationRef.current + 1;
      operationRef.current = operation;
      // A persisted green badge is provisional until the backend task has been
      // read. There is intentionally no stale-green window while the audit runs.
      if (d.status === 'success') patch({ status: 'running', errorMessage: undefined });

      void auditMountedGenerationSnapshot(d).then((audit) => {
        if (!mountedRef.current || operationRef.current !== operation) return;
        if (audit.kind === 'task') {
          if (audit.task.creation_task_id !== d.taskId || !taskBelongsToCard(audit.task)) {
            rejectOutOfScopeTask(d.taskId ?? audit.task.creation_task_id);
          } else if (isTerminal(audit.task.status)) {
            finalize(audit.task, { allowSpawn: false });
          } else {
            patch({ status: mapStatus(audit.task.status), errorMessage: undefined });
            startPolling(audit.task.creation_task_id, allowSpawnAfterMountedSnapshot(d.status));
          }
        } else if (audit.kind === 'task-unavailable' && d.taskId) {
          const allowSpawn = allowSpawnAfterMountedSnapshot(d.status);
          if (allowSpawn) {
            // Preserve cancel/run-lock semantics for a task that was genuinely
            // in flight; the next poll retries the transient verification.
            patch({ status: d.status, taskId: d.taskId });
          } else {
            patch({ status: 'error', taskId: d.taskId, errorMessage: GENERATION_TASK_VERIFICATION_ERROR });
          }
          startPolling(d.taskId, allowSpawn);
        } else if (audit.kind === 'task-missing') {
          activeTaskRef.current = null;
          clearTimer();
          patch({
            status: 'error',
            taskId: null,
            resultAssetIds: [],
            errorMessage: GENERATION_TASK_REQUIRED_ERROR,
            batch: undefined,
          });
        }
      });
    }
    return () => {
      mountedRef.current = false;
      operationRef.current += 1;
      clearTimer();
      activeTaskRef.current = null;
    };
    // Mount-only: resume decision reads the initial data snapshot via ref.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const runVoid = useCallback(() => void run(), [run]);
  return { run: runVoid, cancel };
}
