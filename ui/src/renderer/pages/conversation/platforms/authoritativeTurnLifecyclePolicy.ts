import type { MessageId } from '@/common/types/ids';

export type AuthoritativeTurnStartAction = 'accept' | 'verify_runtime' | 'ignore';

export const classifyAuthoritativeTurnStart = ({
  turnId,
  cancelledTurnIds,
  rejectUnannouncedStart,
  awaitingBackendTurn,
  verifyUnannouncedStartRuntime,
}: {
  turnId?: MessageId;
  cancelledTurnIds: ReadonlySet<MessageId>;
  rejectUnannouncedStart: boolean;
  awaitingBackendTurn: boolean;
  verifyUnannouncedStartRuntime: boolean;
}): AuthoritativeTurnStartAction => {
  if (turnId && cancelledTurnIds.has(turnId)) return 'ignore';
  // With a known stopped root, its tombstone is precise: a different turn id
  // is a genuinely newer turn and must be allowed to invalidate the pending
  // stop continuation. Unknown-root stops retain the runtime-verification
  // barrier because their delayed start cannot yet be distinguished safely.
  if (rejectUnannouncedStart && !awaitingBackendTurn && verifyUnannouncedStartRuntime) return 'ignore';
  if (verifyUnannouncedStartRuntime) return 'verify_runtime';
  return 'accept';
};

export const shouldAcceptAuthoritativeTurnStart = (
  input: Parameters<typeof classifyAuthoritativeTurnStart>[0]
): boolean => classifyAuthoritativeTurnStart(input) === 'accept';

export const resolveVerifiedAuthoritativeTurnStart = ({
  runtimeIsProcessing,
  eventProcessingStartedAt,
  runtimeProcessingStartedAt,
}: {
  runtimeIsProcessing: boolean;
  eventProcessingStartedAt?: number;
  runtimeProcessingStartedAt?: number;
}): 'accept' | 'ignore' => {
  if (!runtimeIsProcessing) return 'ignore';

  // A delayed start from the stopped turn may race a genuinely newer external
  // turn. Both snapshots carry the backend's stable start timestamp when the
  // server supports it; a mismatch means the GET verified a different turn.
  if (
    Number.isFinite(eventProcessingStartedAt) &&
    Number.isFinite(runtimeProcessingStartedAt) &&
    eventProcessingStartedAt !== runtimeProcessingStartedAt
  ) {
    return 'ignore';
  }

  return 'accept';
};

export type AuthoritativeCompletionAction = 'settle' | 'reconcile_runtime' | 'ignore';

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
