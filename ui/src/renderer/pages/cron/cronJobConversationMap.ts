/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ICronJob } from '@/common/adapter/ipcBridge';
import type { ConversationId } from '@/common/types/ids';

export type CronJobsByConversation = Map<ConversationId, ICronJob[]>;

/** Reconcile one conversation-scoped list after a job gains or loses that binding. */
export function reconcileCronJobsForConversation(
  previous: ICronJob[],
  conversationId: ConversationId,
  job: ICronJob,
): ICronJob[] {
  if (job.metadata.conversation_id !== conversationId) {
    return previous.filter((item) => item.cron_job_id !== job.cron_job_id);
  }
  return previous.some((item) => item.cron_job_id === job.cron_job_id)
    ? previous.map((item) => (item.cron_job_id === job.cron_job_id ? job : item))
    : [...previous, job];
}

/** Unbound jobs belong on the task page, not in a conversation-index bucket. */
export function indexCronJobsByConversation(jobs: ICronJob[]): CronJobsByConversation {
  const result: CronJobsByConversation = new Map();
  for (const job of jobs) {
    const conversationId = job.metadata.conversation_id;
    if (!conversationId) continue;
    result.set(conversationId, [...(result.get(conversationId) ?? []), job]);
  }
  return result;
}

/**
 * Replace a job by ID across the index, moving it when lazy binding supplies
 * its first conversation ID and removing it when it becomes unbound.
 */
export function upsertCronJobByConversation(
  previous: CronJobsByConversation,
  job: ICronJob,
): CronJobsByConversation {
  const result = new Map(previous);
  for (const [conversationId, jobs] of result.entries()) {
    const remaining = jobs.filter((item) => item.cron_job_id !== job.cron_job_id);
    if (remaining.length === 0) {
      result.delete(conversationId);
    } else if (remaining.length !== jobs.length) {
      result.set(conversationId, remaining);
    }
  }

  const conversationId = job.metadata.conversation_id;
  if (conversationId) {
    result.set(conversationId, [...(result.get(conversationId) ?? []), job]);
  }
  return result;
}
