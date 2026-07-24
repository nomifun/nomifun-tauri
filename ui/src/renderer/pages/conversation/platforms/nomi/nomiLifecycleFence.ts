import type { MessageId } from '@/common/types/ids';

export type NomiHydrationLifecycleFence = {
  turnClosed: boolean;
  verifyUnannouncedStartRuntime: boolean;
};

/**
 * A fresh idle runtime snapshot closes the previously rendered turn. Every
 * fresh snapshot also lacks the active outer turn id, so a later unannounced
 * turn.started must be checked against a new runtime snapshot before it may
 * establish correlation.
 *
 * A running snapshot stays open: hiding stream activity while the backend says
 * it is processing would only conceal a real lifecycle problem.
 */
export const getNomiHydrationLifecycleFence = (
  isRunning: boolean
): NomiHydrationLifecycleFence => ({
  turnClosed: !isRunning,
  verifyUnannouncedStartRuntime: true,
});

/**
 * Decide whether a response-stream event may mutate the current turn state.
 *
 * `turn_id` is the stable outer-turn identity. `msg_id` deliberately is not
 * used here: Nomi mints distinct message ids for the submitted user row,
 * provider segments, and terminal projections, so treating msg_id as the turn
 * authority would reject valid continuation output.
 *
 * Rejected events may still be projected into the transcript; they simply
 * cannot revive activity, reconcile completion, change thought/tool state, or
 * overwrite metrics for another turn.
 */
export const shouldApplyNomiStreamEventToTurn = ({
  eventTurnId,
  activeTurnId,
  turnClosed,
  awaitingBackendTurn,
}: {
  eventTurnId?: MessageId;
  activeTurnId: MessageId | null;
  turnClosed: boolean;
  awaitingBackendTurn: boolean;
}): boolean => {
  if (turnClosed && !awaitingBackendTurn) return false;
  if (activeTurnId || eventTurnId) {
    return Boolean(activeTurnId && eventTurnId && activeTurnId === eventTurnId);
  }
  return awaitingBackendTurn;
};
