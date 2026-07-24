/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseConversationId, parseCronJobId, parseProviderId } from '@/common/types/ids';
import type { ICronJob } from '@/common/adapter/ipcBridge';
import { filterCronJobsByQuery, filterCronJobsByStatus } from './cronJobSearch';

const cronJobId = (suffix: string) =>
  parseCronJobId(`019b0000-0000-7000-8000-${suffix.padStart(12, '0')}`);

function job(overrides: Partial<ICronJob>): ICronJob {
  return {
    cron_job_id: cronJobId('101'),
    name: 'Daily standup',
    description: 'Summarize project work',
    enabled: true,
    schedule: { kind: 'cron', expr: '0 0 9 * * ?', description: 'Every day at 09:00' },
    message: 'Collect yesterday progress',
    execution_mode: 'new_conversation',
    metadata: {
      conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000101'),
      conversation_title: 'Engineering Room',
      agent_type: 'claude',
      created_by: 'user',
      created_at: 1,
      updated_at: 1,
      agent_config: { backend: 'claude', name: 'Claude Code' },
    },
    state: {
      run_count: 0,
      retry_count: 0,
      max_retries: 0,
    },
    ...overrides,
  };
}

describe('filterCronJobsByQuery', () => {
  const jobs = [
    job({ cron_job_id: cronJobId('101'), name: 'Daily standup' }),
    job({
      cron_job_id: cronJobId('102'),
      name: 'Release notes',
      description: 'Prepare customer changelog',
      schedule: { kind: 'cron', expr: '0 30 17 * * ?', description: 'Every day at 17:30' },
      message: 'Draft the changelog from merged PRs',
      execution_mode: 'existing',
      metadata: {
        conversation_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000102'),
        conversation_title: 'Launch Plan',
        agent_type: 'nomi',
        created_by: 'user',
        created_at: 2,
        updated_at: 2,
        agent_config: {
          provider_id: parseProviderId('0190f5fe-7c00-7a00-8000-000000000201'),
          model: 'nomi-model',
          name: 'Nomi',
        },
      },
    }),
  ];

  test('returns every job for a blank query', () => {
    expect(filterCronJobsByQuery(jobs, '   ')).toEqual(jobs);
  });

  test('matches job metadata, message, schedule, and execution fields case-insensitively', () => {
    expect(filterCronJobsByQuery(jobs, 'launch').map((item) => item.cron_job_id)).toEqual([jobs[1].cron_job_id]);
    expect(filterCronJobsByQuery(jobs, 'MERGED prs').map((item) => item.cron_job_id)).toEqual([jobs[1].cron_job_id]);
    expect(filterCronJobsByQuery(jobs, '09:00').map((item) => item.cron_job_id)).toEqual([jobs[0].cron_job_id]);
  });

  test('searches the full cron UUID and its short suffix without #N semantics', () => {
    expect(filterCronJobsByQuery(jobs, '#2')).toEqual([]);
    expect(filterCronJobsByQuery(jobs, '000000000102').map((item) => item.cron_job_id)).toEqual([jobs[1].cron_job_id]);
    expect(filterCronJobsByQuery(jobs, '019b0000-0000-7000-8000-000000000102').map((item) => item.cron_job_id)).toEqual([
      jobs[1].cron_job_id,
    ]);
    expect(filterCronJobsByQuery(jobs, '#019b0000-0000-7000-8000-000000000102')).toEqual([]);
  });

  test('does not index a placeholder conversation ID for an unbound task', () => {
    const unbound = job({ name: 'Not run yet' });
    unbound.metadata = { ...unbound.metadata, conversation_id: undefined };

    expect(filterCronJobsByQuery([unbound], '#undefined')).toEqual([]);
  });
});

describe('filterCronJobsByStatus', () => {
  const jobs = [
    job({ cron_job_id: cronJobId('111'), enabled: true }),
    job({ cron_job_id: cronJobId('112'), enabled: false }),
  ];

  test('filters enabled and paused jobs while preserving all jobs for the default filter', () => {
    expect(filterCronJobsByStatus(jobs, 'all')).toEqual(jobs);
    expect(filterCronJobsByStatus(jobs, 'active')).toEqual([jobs[0]]);
    expect(filterCronJobsByStatus(jobs, 'paused')).toEqual([jobs[1]]);
  });
});
