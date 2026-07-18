import { describe, expect, test } from 'bun:test';
import { parseMessageId } from '@/common/types/ids';
import {
  COMMAND_QUEUE_RECONCILE_DELAYS_MS,
  getCommandQueueReconcileDelayMs,
  isCommandQueueExecutionCurrent,
  reduceCommandQueueExecutionGate,
  type CommandQueueExecutionGate,
} from './commandQueueExecutionGate';

const oldTurnId = parseMessageId('msg_0190f5fe-7c00-7a00-8000-000000000001');
const newTurnId = parseMessageId('msg_0190f5fe-7c00-7a00-8000-000000000002');

describe('command queue execution gate', () => {
  test('only the mounted matching conversation generation may continue an execution', () => {
    const current = {
      mounted: true,
      currentConversationId: 'conv-current',
      expectedConversationId: 'conv-current',
      currentGeneration: 4,
      expectedGeneration: 4,
    };

    expect(isCommandQueueExecutionCurrent(current)).toBe(true);
    expect(isCommandQueueExecutionCurrent({ ...current, mounted: false })).toBe(false);
    expect(isCommandQueueExecutionCurrent({ ...current, currentConversationId: 'conv-next' })).toBe(false);
    expect(isCommandQueueExecutionCurrent({ ...current, currentGeneration: 5 })).toBe(false);
  });

  test('runtime reconciliation backoff remains bounded while retries continue', () => {
    expect(getCommandQueueReconcileDelayMs(0)).toBe(120);
    expect(getCommandQueueReconcileDelayMs(2)).toBe(1_200);
    expect(getCommandQueueReconcileDelayMs(99)).toBe(COMMAND_QUEUE_RECONCILE_DELAYS_MS.at(-1));
  });

  test('a manual turn.started owns the idle queue until exact completion', () => {
    const running = reduceCommandQueueExecutionGate({ phase: 'idle' }, { type: 'turnStarted', turnId: newTurnId });
    expect(running).toEqual({ phase: 'waiting_completion', turnId: newTurnId });
    expect(
      reduceCommandQueueExecutionGate(running, {
        type: 'turnCompleted',
        turnId: newTurnId,
        runtimeIsProcessing: false,
      })
    ).toEqual({ phase: 'idle' });
  });

  test('visual stream completion has no transition and stop keeps ownership closed', () => {
    const running: CommandQueueExecutionGate = { phase: 'waiting_completion', turnId: newTurnId };
    // There intentionally is no stream-finish event in the state machine.
    expect(reduceCommandQueueExecutionGate(running, { type: 'stop' })).toBe(running);
    expect(reduceCommandQueueExecutionGate({ phase: 'idle' }, { type: 'stop' })).toEqual({
      phase: 'waiting_completion',
    });
  });

  test('accepted send reconciliation recovers a missed turn.started', () => {
    const waiting: CommandQueueExecutionGate = { phase: 'waiting_start' };
    expect(
      reduceCommandQueueExecutionGate(waiting, {
        type: 'runtimeReconciled',
        purpose: 'start',
        runtimeIsProcessing: true,
      })
    ).toEqual({ phase: 'waiting_completion' });
    expect(
      reduceCommandQueueExecutionGate(waiting, {
        type: 'runtimeReconciled',
        purpose: 'start',
        runtimeIsProcessing: false,
      })
    ).toEqual({ phase: 'idle' });
  });

  test('an old completion cannot release a newer or not-yet-acknowledged turn', () => {
    const waitingStart: CommandQueueExecutionGate = { phase: 'waiting_start' };
    expect(
      reduceCommandQueueExecutionGate(waitingStart, {
        type: 'turnCompleted',
        turnId: oldTurnId,
        runtimeIsProcessing: false,
      })
    ).toBe(waitingStart);
    expect(
      reduceCommandQueueExecutionGate(waitingStart, {
        type: 'runtimeReconciled',
        purpose: 'completion',
        runtimeIsProcessing: false,
      })
    ).toBe(waitingStart);

    const newer: CommandQueueExecutionGate = { phase: 'waiting_completion', turnId: newTurnId };
    expect(
      reduceCommandQueueExecutionGate(newer, {
        type: 'turnCompleted',
        turnId: oldTurnId,
        runtimeIsProcessing: false,
      })
    ).toBe(newer);
  });
});
