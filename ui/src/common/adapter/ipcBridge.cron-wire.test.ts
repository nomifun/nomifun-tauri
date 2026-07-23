/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { InvalidEntityIdError, parseCronJobId } from '@/common/types/ids';
import { cron } from './ipcBridge';

const CRON_JOB_ID = '0190f5fe-7c00-7a00-8000-000000000010';
const CRON_JOB_RUN_ID = '0190f5fe-7c00-7a00-8000-000000000011';
const realFetch = globalThis.fetch;

const rawCronJob = (cron_job_id: unknown) => ({
  cron_job_id,
  name: 'Boundary test',
  enabled: true,
  schedule: { kind: 'every', every_ms: 60_000, description: 'Every minute' },
  message: 'Run boundary test',
  execution_mode: 'existing',
  metadata: {
    agent_type: 'nomi',
    created_by: 'user',
    created_at: 1,
    updated_at: 1,
  },
  state: {
    run_count: 0,
    retry_count: 0,
    max_retries: 0,
  },
});

const rawCronJobRun = (cron_job_run_id: unknown, cron_job_id: unknown = CRON_JOB_ID) => ({
  cron_job_run_id,
  cron_job_id,
  executed_at_ms: 1,
  status: 'ok',
});

function respondWith(data: unknown): void {
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ success: true, data }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    )) as unknown as typeof fetch;
}

async function expectInvalidEntityId(action: () => Promise<unknown>): Promise<void> {
  let error: unknown;
  try {
    await action();
  } catch (caught) {
    error = caught;
  }
  expect(error instanceof InvalidEntityIdError).toBe(true);
}

describe('cron response wire ID contract', () => {
  test('accepts canonical UUIDv7 IDs and rejects numeric or legacy-prefixed IDs', async () => {
    try {
      respondWith([rawCronJob(CRON_JOB_ID)]);
      expect((await cron.listJobs.invoke())[0]?.cron_job_id).toBe(CRON_JOB_ID);

      respondWith([rawCronJobRun(CRON_JOB_RUN_ID)]);
      const runs = await cron.listRuns.invoke({ cron_job_id: parseCronJobId(CRON_JOB_ID) });
      expect(runs[0]?.cron_job_run_id).toBe(CRON_JOB_RUN_ID);
      expect(runs[0]?.cron_job_id).toBe(CRON_JOB_ID);

      for (const invalidId of [10, `cron_${CRON_JOB_ID}`]) {
        respondWith([rawCronJob(invalidId)]);
        await expectInvalidEntityId(() => cron.listJobs.invoke());
      }

      for (const invalidRunId of [11, `cronrun_${CRON_JOB_RUN_ID}`]) {
        respondWith([rawCronJobRun(invalidRunId)]);
        await expectInvalidEntityId(() =>
          cron.listRuns.invoke({ cron_job_id: parseCronJobId(CRON_JOB_ID) }),
        );
      }

      for (const invalidJobId of [10, `cron_${CRON_JOB_ID}`]) {
        respondWith([rawCronJobRun(CRON_JOB_RUN_ID, invalidJobId)]);
        await expectInvalidEntityId(() =>
          cron.listRuns.invoke({ cron_job_id: parseCronJobId(CRON_JOB_ID) }),
        );
      }
    } finally {
      globalThis.fetch = realFetch;
    }
  });
});
