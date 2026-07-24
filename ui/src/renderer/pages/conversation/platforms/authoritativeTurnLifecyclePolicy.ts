import type { MessageId } from '@/common/types/ids';

export type AuthoritativeTurnStartAction = 'accept' | 'verify_runtime' | 'ignore';

export const classifyAuthoritativeTurnStart = ({
  turnId,
  activeTurnId,
  cancelledTurnIds,
  rejectUnannouncedStart,
  awaitingBackendTurn,
  verifyUnannouncedStartRuntime,
}: {
  turnId: MessageId;
  activeTurnId?: MessageId | null;
  cancelledTurnIds: ReadonlySet<MessageId>;
  rejectUnannouncedStart: boolean;
  awaitingBackendTurn: boolean;
  verifyUnannouncedStartRuntime: boolean;
}): AuthoritativeTurnStartAction => {
  if (cancelledTurnIds.has(turnId)) return 'ignore';
  // With a known stopped root, its tombstone is precise: a different turn id
  // is a genuinely newer turn and must be allowed to invalidate the pending
  // stop continuation. Unknown-root stops retain the runtime-verification
  // barrier because their delayed start cannot yet be distinguished safely.
  if (rejectUnannouncedStart && !awaitingBackendTurn) {
    // An unknown-root stop cannot distinguish its delayed start, so keep it
    // closed. With a known stopped root the exact tombstone above rejects that
    // root, while a different id still needs exact active_turn_id proof before
    // it may replace the stopped generation.
    return verifyUnannouncedStartRuntime ? 'ignore' : 'verify_runtime';
  }
  if (activeTurnId) {
    // Duplicate starts are not a new lifecycle generation. A conflicting id
    // cannot replace the active root from event order alone: it may be a
    // delayed prior-turn event, so require exact active_turn_id proof.
    return activeTurnId === turnId ? 'ignore' : 'verify_runtime';
  }
  if (verifyUnannouncedStartRuntime) return 'verify_runtime';
  return 'accept';
};

export const shouldAcceptAuthoritativeTurnStart = (
  input: Parameters<typeof classifyAuthoritativeTurnStart>[0]
): boolean => classifyAuthoritativeTurnStart(input) === 'accept';

export const resolveVerifiedAuthoritativeTurnStart = ({
  turnId,
  runtimeIsProcessing,
  eventActiveTurnId,
  runtimeActiveTurnId,
}: {
  turnId: MessageId;
  runtimeIsProcessing: boolean;
  eventActiveTurnId?: MessageId;
  runtimeActiveTurnId?: MessageId;
}): 'accept' | 'ignore' => {
  if (!runtimeIsProcessing) return 'ignore';

  // Runtime "busy" and millisecond timestamps are not operation authority. A
  // delayed old start can race a genuinely newer turn (and two starts may
  // share one millisecond). Accept only when both the event snapshot and a
  // fresh conversation GET name this exact backend-minted active turn.
  return eventActiveTurnId === turnId && runtimeActiveTurnId === turnId
    ? 'accept'
    : 'ignore';
};

export type AuthoritativeCompletionAction = 'settle' | 'reconcile_runtime' | 'ignore';

export const isAuthoritativeCompletionRuntimeIdle = (runtime: {
  is_processing: boolean;
  active_turn_id?: MessageId;
}): boolean =>
  runtime.is_processing === false &&
  runtime.active_turn_id == null;

export const classifyAuthoritativeTurnCompletion = ({
  rootTurnId,
  completedTurnId,
  awaitingBackendTurn,
}: {
  rootTurnId: MessageId | null;
  completedTurnId?: MessageId;
  awaitingBackendTurn: boolean;
}): AuthoritativeCompletionAction => {
  if (rootTurnId && completedTurnId) {
    return rootTurnId === completedTurnId ? 'settle' : 'ignore';
  }
  if (awaitingBackendTurn) return 'ignore';
  return 'reconcile_runtime';
};
