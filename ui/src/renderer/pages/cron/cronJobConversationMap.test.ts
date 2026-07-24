/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { ICronJob } from '@/common/adapter/ipcBridge';
import { parseConversationId, parseCronJobId } from '@/common/types/ids';
import {
  indexCronJobsByConversation,
  reconcileCronJobsForConversation,
  upsertCronJobByConversation,
} from './cronJobConversationMap';

const cronJobId = (sequence: number) =>
  parseCronJobId(`019b0000-0000-7000-8000-${sequence.toString(16).padStart(12, '0')}`);

function job(sequence: number, conversation_id?: ReturnType<typeof parseConversationId>): ICronJob {
  return {
    cron_job_id: cronJobId(sequence),
    name: `job-${sequence}`,
    enabled: true,
    schedule: { kind: 'cron', expr: '', description: '' },
    message: '',
    execution_mode: 'existing',
    metadata: {
      conversation_id,
      agent_type: 'claude',
      created_by: 'user',
      created_at: 1,
      updated_at: 1,
    },
    state: { run_count: 0, retry_count: 0, max_retries: 3 },
  };
}

describe('cron job conversation index', () => {
  const firstConversation = parseConversationId('019b0000-0000-7000-8000-000000000001');
  const secondConversation = parseConversationId('019b0000-0000-7000-8000-000000000002');

  test('skips unbound jobs instead of creating an undefined bucket', () => {
    const bound = job(1, firstConversation);
    const index = indexCronJobsByConversation([job(2), bound]);

    expect([...index.keys()]).toEqual([firstConversation]);
    expect(index.get(firstConversation)).toEqual([bound]);
  });

  test('adds a lazily-bound job and moves later updates by job ID', () => {
    const unbound = job(1);
    const initiallyBound = { ...unbound, metadata: { ...unbound.metadata, conversation_id: firstConversation } };
    const moved = { ...initiallyBound, metadata: { ...initiallyBound.metadata, conversation_id: secondConversation } };

    const afterBinding = upsertCronJobByConversation(new Map(), initiallyBound);
    expect(afterBinding.get(firstConversation)).toEqual([initiallyBound]);

    const afterMove = upsertCronJobByConversation(afterBinding, moved);
    expect(afterMove.has(firstConversation)).toBe(false);
    expect(afterMove.get(secondConversation)).toEqual([moved]);
  });

  test('reconciles a conversation-scoped list when a lazy binding appears or moves away', () => {
    const unbound = job(1);
    const bound = { ...unbound, metadata: { ...unbound.metadata, conversation_id: firstConversation } };

    expect(reconcileCronJobsForConversation([], firstConversation, bound)).toEqual([bound]);
    expect(reconcileCronJobsForConversation([bound], firstConversation, unbound)).toEqual([]);
  });
});
