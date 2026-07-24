import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';
import {
  parseAssetId,
  parseCanvasId,
  parseCreationTaskId,
  parseProviderId,
  parseWorkshopNodeId,
  type AssetId,
  type CreationTaskId,
} from '@/common/types/ids';
import type { CreationTask, WorkshopGeneratorNodeData } from '../types';
import {
  allowSpawnAfterMountedSnapshot,
  auditMountedGenerationSnapshot,
  resolveTerminalGenerationTask,
} from './useGenerationRun';

const taskId = parseCreationTaskId('0190f5fe-7c00-7a00-8000-000000000012');
const canvasId = parseCanvasId('019b0000-0000-7000-8000-000000000002');
const nodeId = parseWorkshopNodeId('019b0000-0000-7000-8000-000000000003');
const providerId = parseProviderId('019b0000-0000-7000-8000-000000000004');
const asset = (label: string): AssetId => {
  const suffix = Array.from(label)
    .map((char) => char.charCodeAt(0).toString(16).padStart(2, '0'))
    .join('')
    .slice(0, 12)
    .padEnd(12, '0');
  return parseAssetId(`019b0000-0000-7000-8000-${suffix}`);
};

function task(patch: Partial<CreationTask> = {}): CreationTask {
  return {
    creation_task_id: taskId,
    canvas_id: canvasId,
    node_id: nodeId,
    provider_id: providerId,
    model: 'model',
    capability: 't2i',
    params: {},
    status: 'succeeded',
    error: null,
    result_asset_ids: [asset('result')],
    attempt: 1,
    submitted_at: 1,
    started_at: 2,
    finished_at: 3,
    ...patch,
  };
}

function snapshot(patch: Partial<WorkshopGeneratorNodeData> = {}): WorkshopGeneratorNodeData {
  return {
    mode: 'image',
    prompt: '',
    params: {},
    mentions: [],
    status: 'success',
    taskId,
    resultAssetIds: [asset('stale')],
    ...patch,
  };
}

describe('generation snapshot reopen audit', () => {
  test('re-fetches a terminal green snapshot and exposes the backend downgrade', async () => {
    let fetched: CreationTaskId | null = null;
    const backendTask = task({
      status: 'failed',
      error: { kind: 'artifact_missing', message: 'artifact missing' },
      result_asset_ids: [],
    });

    const audit = await auditMountedGenerationSnapshot(snapshot(), {
      fetchTask: async (id) => {
        fetched = id;
        return backendTask;
      },
    });

    expect(fetched).toBe(taskId);
    expect(audit.kind).toBe('task');
    if (audit.kind !== 'task') throw new Error('expected task audit');
    const resolution = resolveTerminalGenerationTask(audit.task);
    expect(resolution.patch.status).toBe('error');
    expect(resolution.patch.resultAssetIds).toEqual([]);
    expect(resolution.patch.errorMessage).toBe('artifact missing');
  });

  test('rejects a successful snapshot that has no authoritative task id', async () => {
    const ids = [asset('one'), asset('two')];
    const audit = await auditMountedGenerationSnapshot(
      snapshot({ taskId: null, resultAssetIds: ids })
    );

    expect(audit.kind).toBe('task-missing');
  });

  test('mount reconciliation cannot fan out the same batch on every reopen', () => {
    const source = readFileSync(new URL('./useGenerationRun.ts', import.meta.url), 'utf8');
    expect(source.includes('finalize(audit.task, { allowSpawn: false })')).toBe(true);
    expect(allowSpawnAfterMountedSnapshot('success')).toBe(false);
    expect(allowSpawnAfterMountedSnapshot('error')).toBe(false);
    expect(allowSpawnAfterMountedSnapshot('running')).toBe(true);
    expect(allowSpawnAfterMountedSnapshot('queued')).toBe(true);
  });
});
