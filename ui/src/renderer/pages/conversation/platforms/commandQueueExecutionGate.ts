import type { MessageId } from '@/common/types/ids';

export type CommandQueueExecutionGate =
  | { phase: 'idle' }
  | { phase: 'waiting_start' }
  | { phase: 'waiting_completion'; turnId?: MessageId };

export type CommandQueueExecutionGateEvent =
  | { type: 'begin' }
  | { type: 'turnStarted'; turnId?: MessageId }
  | { type: 'turnCompleted'; turnId?: MessageId; runtimeIsProcessing: boolean }
  | { type: 'runtimeReconciled'; purpose: 'start' | 'completion'; runtimeIsProcessing: boolean }
  | { type: 'stop' }
  | { type: 'reset' };

export const IDLE_EXECUTION_GATE: CommandQueueExecutionGate = { phase: 'idle' };

/** Retry forever while an idle UI still has a non-idle queue gate, but cap the
 * interval so recovery is guaranteed after the status service becomes healthy
 * without creating a tight polling loop during an outage. */
export const COMMAND_QUEUE_RECONCILE_DELAYS_MS = [120, 400, 1_200, 3_000, 8_000, 16_000] as const;

export const getCommandQueueReconcileDelayMs = (attempt: number): number => {
  const index = Math.min(Math.max(0, Math.floor(attempt)), COMMAND_QUEUE_RECONCILE_DELAYS_MS.length - 1);
  return COMMAND_QUEUE_RECONCILE_DELAYS_MS[index];
};

export const isCommandQueueExecutionCurrent = ({
  mounted,
  currentConversationId,
  expectedConversationId,
  currentGeneration,
  expectedGeneration,
}: {
  mounted: boolean;
  currentConversationId: string;
  expectedConversationId: string;
  currentGeneration: number;
  expectedGeneration: number;
}): boolean =>
  mounted &&
  currentConversationId === expectedConversationId &&
  currentGeneration === expectedGeneration;

/**
 * Pure queue ownership state machine. A visual stream terminal is deliberately
 * not an input: only a correlated turn.completed or an authoritative runtime
 * read may release ownership of the per-conversation backend turn handle.
 */
export const reduceCommandQueueExecutionGate = (
  gate: CommandQueueExecutionGate,
  event: CommandQueueExecutionGateEvent
): CommandQueueExecutionGate => {
  switch (event.type) {
    case 'begin':
      return gate.phase === 'idle' ? { phase: 'waiting_start' } : gate;
    case 'turnStarted':
      return { phase: 'waiting_completion', turnId: event.turnId };
    case 'turnCompleted':
      if (
        event.runtimeIsProcessing ||
        gate.phase !== 'waiting_completion' ||
        !gate.turnId ||
        !event.turnId ||
        gate.turnId !== event.turnId
      ) {
        return gate;
      }
      return IDLE_EXECUTION_GATE;
    case 'runtimeReconciled':
      if (event.purpose === 'start') {
        if (gate.phase !== 'waiting_start') return gate;
        return event.runtimeIsProcessing ? { phase: 'waiting_completion' } : IDLE_EXECUTION_GATE;
      }
      if (gate.phase !== 'waiting_completion' || event.runtimeIsProcessing) return gate;
      return IDLE_EXECUTION_GATE;
    case 'stop':
      // Optimistic UI cancellation must not open the queue before the backend
      // has actually released its turn handle.
      return gate.phase === 'idle' ? { phase: 'waiting_completion' } : gate;
    case 'reset':
      return IDLE_EXECUTION_GATE;
    default:
      return gate;
  }
};
