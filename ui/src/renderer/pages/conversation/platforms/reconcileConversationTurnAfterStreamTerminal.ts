import type { ConversationId } from '@/common/types/ids';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { getConversationRuntimeAuthority } from '@/renderer/pages/conversation/utils/conversationRuntime';

export const TERMINAL_RECONCILE_DELAYS_MS = [120, 400, 1_200, 3_000, 8_000, 16_000] as const;
export const TERMINAL_RECONCILE_QUERY_TIMEOUT_MS = 3_000;

export const terminalReconcileDelayForAttempt = (
  attempt: number,
  delaysMs: readonly number[] = TERMINAL_RECONCILE_DELAYS_MS
): number => {
  const schedule = delaysMs.length > 0 ? delaysMs : TERMINAL_RECONCILE_DELAYS_MS;
  const boundedAttempt = Math.min(Math.max(0, Math.trunc(attempt)), schedule.length - 1);
  return schedule[boundedAttempt]!;
};

const getConversationWithTimeout = async (
  conversationId: ConversationId,
  getConversation: typeof getConversationOrNull,
  timeoutMs: number
) => {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      getConversation(conversationId),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`Conversation runtime reconciliation timed out after ${timeoutMs}ms`)),
          timeoutMs
        );
      }),
    ]);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
};

/**
 * Reliability reconciliation for a lost turn.completed event. A stream terminal is
 * only a trigger for these reads; it never directly lowers the busy state.
 */
export const reconcileConversationTurnAfterStreamTerminal = async (
  conversationId: ConversationId,
  isCurrent: () => boolean,
  onIdle: () => void,
  delaysMs: readonly number[] = TERMINAL_RECONCILE_DELAYS_MS,
  getConversation: typeof getConversationOrNull = getConversationOrNull,
  queryTimeoutMs = TERMINAL_RECONCILE_QUERY_TIMEOUT_MS,
  retryForever = delaysMs === TERMINAL_RECONCILE_DELAYS_MS,
  onProcessing?: () => void
): Promise<boolean> => {
  let attempt = 0;
  while (retryForever || attempt < delaysMs.length) {
    if (!isCurrent()) return false;
    const delayMs = terminalReconcileDelayForAttempt(attempt, delaysMs);
    attempt += 1;
    await new Promise<void>((resolve) => setTimeout(resolve, delayMs));
    if (!isCurrent()) return false;

    try {
      const conversation = await getConversationWithTimeout(conversationId, getConversation, queryTimeoutMs);
      if (!isCurrent()) return false;
      const runtimeAuthority = getConversationRuntimeAuthority(conversation);
      if (runtimeAuthority === 'processing') {
        onProcessing?.();
        continue;
      }
      if (runtimeAuthority === 'unknown') continue;
      onIdle();
      return true;
    } catch (error) {
      console.warn('[conversation-turn-lifecycle] Failed to reconcile terminal stream:', error);
    }
  }
  return false;
};

/**
 * Reconcile an accepted replay without declaring a new local turn.
 *
 * Only a fresh runtime GET may reopen rendering/processing through
 * `onProcessing`; an idle snapshot settles immediately. Reads retry forever in
 * production so a lost response cannot strand either state.
 */
export const reconcileConversationTurnAfterAcceptedReplay = (
  conversationId: ConversationId,
  isCurrent: () => boolean,
  onProcessing: () => void,
  onIdle: () => void,
  delaysMs: readonly number[] = TERMINAL_RECONCILE_DELAYS_MS,
  getConversation: typeof getConversationOrNull = getConversationOrNull,
  queryTimeoutMs = TERMINAL_RECONCILE_QUERY_TIMEOUT_MS,
  retryForever = delaysMs === TERMINAL_RECONCILE_DELAYS_MS
): Promise<boolean> =>
  reconcileConversationTurnAfterStreamTerminal(
    conversationId,
    isCurrent,
    onIdle,
    delaysMs,
    getConversation,
    queryTimeoutMs,
    retryForever,
    onProcessing
  );
