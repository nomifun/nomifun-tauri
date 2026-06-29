/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TChatConversation } from '@/common/config/storage';
import type { TRunDetail } from '@/common/types/orchestrator/orchestratorTypes';
import { useLeadThinking, type LeadThinkingState } from '@/renderer/pages/orchestrator/useLeadThinking';
import { useRunLive } from '@/renderer/pages/orchestrator/useRunLive';

/**
 * Resolved orchestration-run view for a single conversation.
 *
 * `runId` is the conversation's linked run (`null` when none); `detail` /
 * `leadThinking` / `loading` are the composed live state from {@link useRunLive}
 * and {@link useLeadThinking}, and `refetch` forwards the run-detail refetch.
 */
export interface ConversationRunState {
  runId: string | null;
  detail: TRunDetail | null;
  refetch: () => Promise<void>;
  leadThinking: LeadThinkingState;
  loading: boolean;
}

/**
 * Derives the orchestration run linked to a conversation and composes the
 * existing live hooks against it.
 *
 * The run link lives at `conversation.extra.orchestrator_run_id`, written by the
 * backend (Tasks B1–B3) and refreshed when the conversation is refetched on
 * `conversation.listChanged` — so this hook does NOT subscribe to listChanged
 * itself. It only derives `runId` from the (already up-to-date) conversation and
 * threads it into {@link useRunLive} (run detail + run-engine WS refetch) and
 * {@link useLeadThinking} (lead-agent planning stream).
 *
 * The field only exists on the `nomi` variant of the discriminated union, so we
 * narrow on `conversation.type === 'nomi'` before reading it (no `as any`). When
 * there is no run, `runId` is `null` and both hooks are called with their silent
 * sentinels (`useRunLive(undefined)` / `useLeadThinking(null)`) so they hold no
 * subscriptions. Both hooks are always called (Rules of Hooks); only their
 * argument changes.
 */
export function useConversationRun(conversation: TChatConversation | null | undefined): ConversationRunState {
  const runId = conversation?.type === 'nomi' ? conversation.extra.orchestrator_run_id ?? null : null;

  const { detail, loading, refetch } = useRunLive(runId ?? undefined);
  const leadThinking = useLeadThinking(runId);

  return { runId, detail, refetch, leadThinking, loading };
}
