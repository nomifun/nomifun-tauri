/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * The generation card's run engine: build the request, submit it, poll to a
 * terminal state, and reflect every transition back into node `data` (through
 * the canvas `updateNodeData`, so history + autosave stay consistent). Polling
 * survives remounts — a card whose `data.taskId` is still non-terminal on mount
 * resumes polling — and is torn down on unmount / canvas close.
 */

import { useCallback, useEffect, useRef } from 'react';
import { useReactFlow } from '@xyflow/react';
import { useTranslation } from 'react-i18next';
import { cancelTask, createTask, getTask } from '../api';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import type { CreationTask, CreationTaskStatus, WorkshopGeneratorNodeData, WorkshopGeneratorStatus } from '../types';
import { buildTaskParams } from './genConstants';
import type { ModelOption } from './genTypes';
import { buildRunPlan } from './pipeline';
import { spawnResultNodes } from './spawn';
import { normalizeImageParamsForModel, validateLocalZImageRun } from './localZImage';

const POLL_INTERVAL_MS = 2000;

const TERMINAL: CreationTaskStatus[] = ['succeeded', 'failed', 'canceled'];
const isTerminal = (s: CreationTaskStatus): boolean => TERMINAL.includes(s);

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

export interface UseGenerationRunArgs {
  nodeId: string;
  canvasId: string;
  data: WorkshopGeneratorNodeData;
  /** The model a run should use (explicit selection, else first available). */
  effectiveModel: ModelOption | null;
  updateNodeData: (nodeId: string, patch: Partial<WorkshopGeneratorNodeData>) => void;
}

export interface GenerationRun {
  run: () => void;
  cancel: () => void;
}

export function useGenerationRun(args: UseGenerationRunArgs): GenerationRun {
  const { nodeId, canvasId } = args;
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();
  const { t } = useTranslation();

  // Latest-value refs so the imperative loop never reads stale props.
  const dataRef = useRef(args.data);
  dataRef.current = args.data;
  const modelRef = useRef(args.effectiveModel);
  modelRef.current = args.effectiveModel;
  const updateRef = useRef(args.updateNodeData);
  updateRef.current = args.updateNodeData;

  const mountedRef = useRef(true);
  const timerRef = useRef<number | null>(null);
  const activeTaskRef = useRef<string | null>(null);
  const spawnedTaskRef = useRef<string | null>(null);

  const patch = useCallback((p: Partial<WorkshopGeneratorNodeData>) => updateRef.current(nodeId, p), [nodeId]);

  const clearTimer = useCallback(() => {
    if (timerRef.current != null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  }, []);

  const finalize = useCallback(
    (task: CreationTask) => {
      activeTaskRef.current = null;
      clearTimer();
      if (task.status === 'succeeded') {
        const results = task.result_asset_ids ?? [];
        patch({
          status: 'success',
          taskId: task.id,
          resultAssetIds: results,
          errorMessage: undefined,
          batch: results.length > 1 ? { expanded: true, primary: results[0] } : undefined,
        });
        // Fan the extra images out as nodes exactly once, on the live transition.
        if (results.length > 1 && spawnedTaskRef.current !== task.id) {
          spawnedTaskRef.current = task.id;
          const card = rf.getNode(nodeId);
          if (card) spawnResultNodes(rf, card, results.slice(1));
        }
      } else if (task.status === 'failed') {
        patch({ status: 'error', taskId: task.id, errorMessage: task.error?.message || 'error' });
      } else {
        // canceled
        patch({ status: 'idle', taskId: null });
      }
    },
    [clearTimer, patch, rf, nodeId]
  );

  const poll = useCallback(
    async (taskId: string) => {
      let task: CreationTask;
      try {
        task = await getTask(taskId);
      } catch {
        // Transient fetch error — retry on the next tick if still the active task.
        if (mountedRef.current && activeTaskRef.current === taskId) {
          timerRef.current = window.setTimeout(() => void poll(taskId), POLL_INTERVAL_MS);
        }
        return;
      }
      if (!mountedRef.current || activeTaskRef.current !== taskId) return;
      if (isTerminal(task.status)) {
        finalize(task);
        return;
      }
      patch({ status: mapStatus(task.status) });
      timerRef.current = window.setTimeout(() => void poll(taskId), POLL_INTERVAL_MS);
    },
    [finalize, patch]
  );

  const startPolling = useCallback(
    (taskId: string) => {
      activeTaskRef.current = taskId;
      clearTimer();
      timerRef.current = window.setTimeout(() => void poll(taskId), POLL_INTERVAL_MS);
    },
    [clearTimer, poll]
  );

  const run = useCallback(async () => {
    const d = dataRef.current;
    const model = modelRef.current;
    if (!model) return;
    if (d.status === 'queued' || d.status === 'running') return;

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

    const issue = validateLocalZImageRun(model, plan.capability, plan.inputs);
    if (issue) {
      patch({
        status: 'error',
        taskId: null,
        errorMessage: t('workshopGeneration.run.localTextToImageOnly', {
          defaultValue: '本地 Z-Image 当前仅支持纯文字生成图片，请移除图片输入、蒙版和图片引用后重试。',
        }),
      });
      return;
    }

    const storedParams = d.mode === 'image' ? normalizeImageParamsForModel(model, d.params ?? {}) : (d.params ?? {});
    const params = buildTaskParams(d.mode, storedParams, plan.prompt);
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
      if (!mountedRef.current) return;
      patch({ taskId: task.id, status: mapStatus(task.status) });
      if (isTerminal(task.status)) finalize(task);
      else startPolling(task.id);
    } catch (e) {
      if (mountedRef.current) patch({ status: 'error', errorMessage: e instanceof Error ? e.message : String(e) });
    }
  }, [rf, nodeId, canvasId, patch, finalize, startPolling, t]);

  const cancel = useCallback(() => {
    const taskId = activeTaskRef.current ?? dataRef.current.taskId ?? null;
    clearTimer();
    activeTaskRef.current = null;
    patch({ status: 'idle', taskId: null });
    if (taskId) void cancelTask(taskId).catch(() => {});
  }, [clearTimer, patch]);

  // Resume a still-running task after a remount / canvas reopen (once).
  useEffect(() => {
    mountedRef.current = true;
    const d = dataRef.current;
    if (d.taskId && (d.status === 'queued' || d.status === 'running')) {
      startPolling(d.taskId);
    }
    return () => {
      mountedRef.current = false;
      clearTimer();
      activeTaskRef.current = null;
    };
    // Mount-only: resume decision reads the initial data snapshot via ref.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const runVoid = useCallback(() => void run(), [run]);
  return { run: runVoid, cancel };
}
