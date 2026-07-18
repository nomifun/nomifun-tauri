import type { ConversationId } from '@/common/types/ids';
import { useCallback, useEffect, useRef } from 'react';

export type ConversationStopAttemptToken = {
  conversationId: ConversationId;
  generation: number;
  turnStartGeneration: number;
  turnCompletionGeneration: number;
};

export type ConversationStopAttemptGuardState = {
  conversationId: ConversationId;
  generation: number;
  mounted: boolean;
};

export type ConversationStopAttemptStatus =
  | 'current'
  | 'turn_started'
  | 'turn_completed'
  | 'superseded'
  | 'stale_scope';

/** A newer authoritative turn boundary owns the visible interaction state.
 * The stale stop continuation must only release its stop-button lock; it must
 * never restore or clear the newer lifecycle state. */
export const shouldReleaseStopInteraction = (status: ConversationStopAttemptStatus): boolean =>
  status === 'turn_started' || status === 'turn_completed';

export const createConversationStopAttemptGuardState = (
  conversationId: ConversationId
): ConversationStopAttemptGuardState => ({ conversationId, generation: 0, mounted: false });

export const advanceConversationStopAttemptGuard = (
  state: ConversationStopAttemptGuardState,
  update?: { conversationId?: ConversationId; mounted?: boolean }
): ConversationStopAttemptGuardState => ({
  conversationId: update?.conversationId ?? state.conversationId,
  generation: state.generation + 1,
  mounted: update?.mounted ?? state.mounted,
});

export const isConversationStopAttemptCurrent = (
  state: ConversationStopAttemptGuardState,
  token: ConversationStopAttemptToken,
  turnStartGeneration: number,
  turnCompletionGeneration: number
): boolean =>
  getConversationStopAttemptStatus(
    state,
    token,
    turnStartGeneration,
    turnCompletionGeneration
  ) === 'current';

export const getConversationStopAttemptStatus = (
  state: ConversationStopAttemptGuardState,
  token: ConversationStopAttemptToken,
  turnStartGeneration: number,
  turnCompletionGeneration: number
): ConversationStopAttemptStatus => {
  if (!state.mounted || state.conversationId !== token.conversationId) return 'stale_scope';
  if (state.generation !== token.generation) return 'superseded';
  if (turnStartGeneration !== token.turnStartGeneration) return 'turn_started';
  if (turnCompletionGeneration !== token.turnCompletionGeneration) return 'turn_completed';
  return 'current';
};

export const unmountConversationStopAttemptGuard = (
  state: ConversationStopAttemptGuardState,
  effectConversationId: ConversationId
): ConversationStopAttemptGuardState =>
  state.conversationId === effectConversationId
    ? advanceConversationStopAttemptGuard(state, { mounted: false })
    : state;

/** Invalidates pending stop continuations synchronously on conversation render
 * changes and on unmount, before they can mutate the next conversation. */
export const useConversationStopAttemptGuard = (
  conversationId: ConversationId,
  getTurnStartGeneration: () => number,
  getTurnCompletionGeneration: () => number
) => {
  const stateRef = useRef({
    ...createConversationStopAttemptGuardState(conversationId),
    mounted: true,
  });

  if (stateRef.current.conversationId !== conversationId) {
    stateRef.current = advanceConversationStopAttemptGuard(stateRef.current, {
      conversationId,
      mounted: true,
    });
  }

  useEffect(() => {
    // Do not advance the generation in setup: a click can legitimately happen
    // after commit but before passive effects flush. The render-time identity
    // change already invalidated the previous conversation synchronously.
    stateRef.current = { ...stateRef.current, mounted: true };
    return () => {
      // On a dependency change, the old cleanup runs after render already moved
      // the ref to the next conversation. Do not invalidate a fresh attempt.
      stateRef.current = unmountConversationStopAttemptGuard(stateRef.current, conversationId);
    };
  }, [conversationId]);

  const beginStopAttempt = useCallback((): ConversationStopAttemptToken => {
    stateRef.current = advanceConversationStopAttemptGuard(stateRef.current);
    return {
      conversationId: stateRef.current.conversationId,
      generation: stateRef.current.generation,
      turnStartGeneration: getTurnStartGeneration(),
      turnCompletionGeneration: getTurnCompletionGeneration(),
    };
  }, [getTurnCompletionGeneration, getTurnStartGeneration]);

  const isStopAttemptCurrent = useCallback(
    (token: ConversationStopAttemptToken): boolean =>
      isConversationStopAttemptCurrent(
        stateRef.current,
        token,
        getTurnStartGeneration(),
        getTurnCompletionGeneration()
      ),
    [getTurnCompletionGeneration, getTurnStartGeneration]
  );

  const getStopAttemptStatus = useCallback(
    (token: ConversationStopAttemptToken): ConversationStopAttemptStatus =>
      getConversationStopAttemptStatus(
        stateRef.current,
        token,
        getTurnStartGeneration(),
        getTurnCompletionGeneration()
      ),
    [getTurnCompletionGeneration, getTurnStartGeneration]
  );

  return { beginStopAttempt, isStopAttemptCurrent, getStopAttemptStatus };
};
