/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Loop run coordinator (M8) — the "one-click run the whole chain" engine behind
 * the loop node.
 *
 * For each round `i` (1..count) it drives one generation of the target card by
 * **reusing the generation pipeline** (`buildRunPlan` + `createTask` + poll),
 * without touching the target card's own state machine — round tasks still record
 * `node_id = target card id`, so they show up in the card's task history. Per
 * round it:
 *   1. slices the target's upstream image inputs to the window
 *      `[start-1 + (i-1)*batch, +batch)` (口播稿 起始计数/批次 semantics);
 *   2. prepends the rendered count template to the prompt (`{i}` ⇒ round no.);
 *   3. spawns the results to the target's right on a per-round grid row
 *      (row = round), wired from the target card;
 *   4. reports progress into the {@link ./loopRegistry} so the UI can animate and
 *      the run survives node remounts.
 *
 * Serial mode awaits each round before dispatching the next; parallel mode uses a
 * rolling window of {@link LOOP_PARALLEL_LIMIT}. Aborting cancels the in-flight
 * task(s) and stops scheduling further rounds; failed rounds are recorded and the
 * run continues, with a summary once every round settles.
 */

import type { ReactFlowInstance } from '@xyflow/react';
import { cancelTask, createTask, getTask } from '../../api';
import {
  makeImageNode,
  makeTextNode,
  makeVideoNode,
  newEdgeId,
  type WorkshopFlowEdge,
  type WorkshopFlowNode,
} from '../../canvas/model';
import type { CreationTask, CreationTaskStatus, WorkshopGeneratorMode, WorkshopGeneratorNodeData } from '../../types';
import { buildTaskParams } from '../genConstants';
import type { ModelOption } from '../genTypes';
import { buildRunPlan, loadWorkshopText } from '../pipeline';
import {
  beginLoopRun,
  endLoopRun,
  patchLoopProgress,
  recordLoopRound,
} from './loopRegistry';
import { injectCount, LOOP_PARALLEL_LIMIT, LOOP_POLL_INTERVAL_MS, type LoopConfig, type LoopRoundResult } from './loopTypes';

type RF = ReactFlowInstance<WorkshopFlowNode, WorkshopFlowEdge>;

const RESULT_CELL = 176;
const RESULT_GAP = 22;
const RESULT_RIGHT_GAP = 72;

const TERMINAL: CreationTaskStatus[] = ['succeeded', 'failed', 'canceled'];
const isTerminal = (s: CreationTaskStatus): boolean => TERMINAL.includes(s);

export interface StartLoopArgs {
  rf: RF;
  loopId: string;
  targetId: string;
  canvasId: string;
  config: LoopConfig;
  model: ModelOption;
}

interface RunContext extends StartLoopArgs {
  signal: AbortSignal;
}

/** Resolve a node's absolute canvas position (accounting for a group parent). */
function absolutePosition(rf: RF, node: WorkshopFlowNode): { x: number; y: number } {
  if (node.parentId) {
    const parent = rf.getNode(node.parentId);
    if (parent) return { x: parent.position.x + node.position.x, y: parent.position.y + node.position.y };
  }
  return { x: node.position.x, y: node.position.y };
}

/** Fan a round's result nodes out (already positioned) and wire them from the card. */
function spawnRoundResults(rf: RF, target: WorkshopFlowNode, nodes: WorkshopFlowNode[]): void {
  if (nodes.length === 0) return;
  rf.addNodes(nodes);
  rf.addEdges(nodes.map((n) => ({ id: newEdgeId(), source: target.id, target: n.id })));
}

function placeRoundNode(
  rf: RF,
  target: WorkshopFlowNode,
  round: number,
  col: number,
  factory: (pos: { x: number; y: number }) => WorkshopFlowNode
): WorkshopFlowNode {
  const origin = absolutePosition(rf, target);
  const width = target.width ?? target.measured?.width ?? 344;
  const x = origin.x + width + RESULT_RIGHT_GAP + col * (RESULT_CELL + RESULT_GAP);
  const y = origin.y + (round - 1) * (RESULT_CELL + RESULT_GAP);
  return factory({ x, y });
}

function delay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise<void>((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const timer = window.setTimeout(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    const onAbort = (): void => {
      window.clearTimeout(timer);
      resolve();
    };
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

/** Poll a task to a terminal state, cancelling it if the run is aborted. */
async function pollToTerminal(taskId: string, signal: AbortSignal): Promise<CreationTask | null> {
  // eslint-disable-next-line no-constant-condition
  while (true) {
    if (signal.aborted) {
      void cancelTask(taskId).catch(() => {});
      return null;
    }
    let task: CreationTask;
    try {
      task = await getTask(taskId);
    } catch {
      await delay(LOOP_POLL_INTERVAL_MS, signal);
      continue;
    }
    if (isTerminal(task.status)) return task;
    await delay(LOOP_POLL_INTERVAL_MS, signal);
  }
}

/** Run a single round end-to-end; returns its terminal result. */
async function executeRound(ctx: RunContext, round: number): Promise<LoopRoundResult> {
  const { rf, targetId, canvasId, config, model, signal } = ctx;
  if (signal.aborted) return 'canceled';
  patchLoopProgress(ctx.loopId, { activeRound: round });

  const target = rf.getNode(targetId);
  if (!target || target.type !== 'generator') return 'failed';
  const data = target.data as WorkshopGeneratorNodeData;
  const mode: WorkshopGeneratorMode = data.mode ?? 'image';

  const offset = config.start - 1 + (round - 1) * config.batch;
  const promptPrefix = injectCount(config.countTemplate, round);

  let plan;
  try {
    plan = await buildRunPlan({
      node: target,
      nodes: rf.getNodes(),
      edges: rf.getEdges(),
      mode,
      mentions: data.mentions ?? [],
      maskAssetId: data.maskAssetId,
      basePrompt: data.prompt ?? '',
      imageWindow: { offset, size: config.batch },
      promptPrefix,
    });
  } catch {
    return 'failed';
  }
  if (signal.aborted) return 'canceled';

  const params = buildTaskParams(mode, data.params ?? {}, plan.prompt);
  let task: CreationTask;
  try {
    task = await createTask({
      canvas_id: canvasId,
      node_id: targetId,
      provider_id: model.providerId,
      model: model.model,
      capability: plan.capability,
      params,
      inputs: plan.inputs,
    });
  } catch {
    return 'failed';
  }

  const final = isTerminal(task.status) ? task : await pollToTerminal(task.id, signal);
  if (!final) return 'canceled';
  if (final.status !== 'succeeded') return final.status === 'canceled' ? 'canceled' : 'failed';

  const results = final.result_asset_ids ?? [];
  if (results.length) {
    if (mode === 'text') {
      const texts = await Promise.all(results.map((id) => loadWorkshopText(id)));
      const created = texts
        .map((text, col) => (text ? placeRoundNode(rf, target, round, col, (pos) => makeTextNode(pos, { content: text })) : null))
        .filter((n): n is WorkshopFlowNode => n !== null);
      spawnRoundResults(rf, target, created);
    } else {
      const factory = (assetId: string) =>
        mode === 'video'
          ? (pos: { x: number; y: number }) => makeVideoNode(pos, { assetId })
          : (pos: { x: number; y: number }) => makeImageNode(pos, { assetId });
      const created = results.map((assetId, col) => placeRoundNode(rf, target, round, col, factory(assetId)));
      spawnRoundResults(rf, target, created);
    }
  }
  return 'success';
}

/** Run rounds in parallel with a rolling concurrency window. */
async function runPool(
  rounds: number[],
  limit: number,
  worker: (round: number) => Promise<void>
): Promise<void> {
  let cursor = 0;
  const runners = new Array(Math.min(limit, rounds.length)).fill(0).map(async () => {
    while (cursor < rounds.length) {
      const round = rounds[cursor];
      cursor += 1;
      await worker(round);
    }
  });
  await Promise.all(runners);
}

/**
 * Start a loop run. No-op (returns false) when a run is already in flight for this
 * loop id. The run drives itself to completion via the registry; callers observe
 * progress through {@link subscribeLoop}.
 */
export function startLoopRun(args: StartLoopArgs): boolean {
  const signal = beginLoopRun(args.loopId, args.config.count);
  if (!signal) return false;
  const ctx: RunContext = { ...args, signal };
  void (async () => {
    const rounds = Array.from({ length: ctx.config.count }, (_, i) => i + 1);
    const runOne = async (round: number): Promise<void> => {
      const result = await executeRound(ctx, round);
      if (result === 'canceled') return;
      recordLoopRound(ctx.loopId, round, result);
    };
    try {
      if (ctx.config.loopMode === 'parallel') {
        await runPool(rounds, LOOP_PARALLEL_LIMIT, runOne);
      } else {
        for (const round of rounds) {
          if (ctx.signal.aborted) break;
          await runOne(round);
        }
      }
    } finally {
      endLoopRun(ctx.loopId, ctx.signal.aborted ? 'canceled' : 'done');
    }
  })();
  return true;
}
