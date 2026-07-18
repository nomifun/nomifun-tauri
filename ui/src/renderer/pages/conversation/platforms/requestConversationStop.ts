import { ipcBridge } from '@/common';
import type { ConversationId } from '@/common/types/ids';
import type { TChatConversation } from '@/common/config/storage';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { isConversationProcessing } from '@/renderer/pages/conversation/utils/conversationRuntime';

export const CONVERSATION_STOP_TIMEOUT_MS = 8_000;
export const CONVERSATION_STOP_CONFIRM_TIMEOUT_MS = 8_000;

export class ConversationStopTimeoutError extends Error {
  constructor(timeoutMs: number) {
    super(`Conversation stop request timed out after ${timeoutMs}ms`);
    this.name = 'ConversationStopTimeoutError';
  }
}

export class ConversationStopConfirmationTimeoutError extends Error {
  constructor(timeoutMs: number) {
    super(`Conversation stop confirmation timed out after ${timeoutMs}ms`);
    this.name = 'ConversationStopConfirmationTimeoutError';
  }
}

const withTimeout = async <T>(operation: Promise<T>, timeoutMs: number, timeoutError: () => Error): Promise<T> => {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => reject(timeoutError()), timeoutMs);
      }),
    ]);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
};

export type ConversationTurnReleaseResult = 'released' | 'deleted' | 'processing';

const STOP_RELEASE_RECONCILE_DELAYS_MS = [0, 120, 400, 1_200, 3_000] as const;

/**
 * Confirm that manual sending is safe again. `null` is a real 404/deleted
 * conversation; transport/query failures are intentionally allowed to throw so
 * callers can conservatively restore busy state instead of confusing the two.
 */
export const waitForConversationTurnRelease = async (
  conversationId: ConversationId,
  getConversation: (conversationId: ConversationId) => Promise<TChatConversation | null> = getConversationOrNull,
  delaysMs: readonly number[] = STOP_RELEASE_RECONCILE_DELAYS_MS
): Promise<ConversationTurnReleaseResult> => {
  for (const delayMs of delaysMs) {
    if (delayMs > 0) await new Promise<void>((resolve) => setTimeout(resolve, delayMs));
    const conversation = await getConversation(conversationId);
    if (!conversation) return 'deleted';
    if (!isConversationProcessing(conversation)) return 'released';
  }
  return 'processing';
};

/** Bound the user-facing stop action even if an HTTP/IPC transport never
 * settles. The underlying request remains best-effort, while UI state and queue
 * ownership are reconciled independently against the authoritative runtime. */
export const requestConversationStop = async (
  conversationId: ConversationId,
  timeoutMs = CONVERSATION_STOP_TIMEOUT_MS,
  invoke: (params: { conversation_id: ConversationId }) => Promise<unknown> =
    ipcBridge.conversation.stop.invoke
): Promise<void> => {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      invoke({ conversation_id: conversationId }),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => reject(new ConversationStopTimeoutError(timeoutMs)), timeoutMs);
      }),
    ]);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
};

export type ConfirmedConversationStopResult =
  | { status: 'released' | 'deleted'; requestError?: unknown }
  | { status: 'processing'; requestError?: unknown }
  | { status: 'unknown'; error: unknown; requestError?: unknown };

/** Request cancellation and independently verify the resulting runtime. A
 * timed-out/failed request is still considered successful when GET proves the
 * turn was released or the conversation was deleted. */
export const stopConversationAndConfirmRelease = async (
  conversationId: ConversationId,
  options?: {
    requestStop?: (conversationId: ConversationId) => Promise<void>;
    waitForRelease?: (conversationId: ConversationId) => Promise<ConversationTurnReleaseResult>;
    confirmationTimeoutMs?: number;
  }
): Promise<ConfirmedConversationStopResult> => {
  let requestError: unknown;
  try {
    await (options?.requestStop ?? requestConversationStop)(conversationId);
  } catch (error) {
    requestError = error;
  }

  try {
    const status = await withTimeout(
      (options?.waitForRelease ?? waitForConversationTurnRelease)(conversationId),
      options?.confirmationTimeoutMs ?? CONVERSATION_STOP_CONFIRM_TIMEOUT_MS,
      () =>
        new ConversationStopConfirmationTimeoutError(
          options?.confirmationTimeoutMs ?? CONVERSATION_STOP_CONFIRM_TIMEOUT_MS
        )
    );
    return requestError === undefined ? { status } : { status, requestError };
  } catch (error) {
    return requestError === undefined
      ? { status: 'unknown', error }
      : { status: 'unknown', error, requestError };
  }
};
