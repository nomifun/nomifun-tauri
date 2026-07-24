import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId, parseMessageId } from '@/common/types/ids';
import {
  TERMINAL_RECONCILE_DELAYS_MS,
  reconcileConversationTurnAfterAcceptedReplay,
  reconcileConversationTurnAfterStreamTerminal,
  terminalReconcileDelayForAttempt,
} from './reconcileConversationTurnAfterStreamTerminal';

const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000041');
const activeTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000042');
const idleConversation = {
  status: 'finished',
  runtime: { is_processing: false },
} as TChatConversation;
const busyConversation = {
  status: 'running',
  runtime: { is_processing: true, active_turn_id: activeTurnId },
} as TChatConversation;
const unknownConversation = {
  status: 'running',
  runtime: { is_processing: true },
} as TChatConversation;

describe('terminal stream runtime reconciliation', () => {
  test('times out a hung read and advances to a later authoritative idle read', async () => {
    let reads = 0;
    let idleCalls = 0;
    const result = await reconcileConversationTurnAfterStreamTerminal(
      conversationId,
      () => true,
      () => {
        idleCalls += 1;
      },
      [0, 0],
      async () => {
        reads += 1;
        if (reads === 1) return new Promise<never>(() => {});
        return idleConversation;
      },
      5
    );

    expect(result).toBe(true);
    expect(reads).toBe(2);
    expect(idleCalls).toBe(1);
  });

  test('reuses the capped production delay after the initial schedule is exhausted', () => {
    expect(terminalReconcileDelayForAttempt(0)).toBe(TERMINAL_RECONCILE_DELAYS_MS[0]);
    expect(terminalReconcileDelayForAttempt(TERMINAL_RECONCILE_DELAYS_MS.length - 1)).toBe(16_000);
    expect(terminalReconcileDelayForAttempt(TERMINAL_RECONCILE_DELAYS_MS.length + 100)).toBe(16_000);
  });

  test('a forever retry stops when its generation is no longer current', async () => {
    let reads = 0;
    let idleCalls = 0;
    const result = await reconcileConversationTurnAfterStreamTerminal(
      conversationId,
      () => reads < 2,
      () => {
        idleCalls += 1;
      },
      [0],
      async () => {
        reads += 1;
        return busyConversation;
      },
      5,
      true
    );

    expect(result).toBe(false);
    expect(reads).toBe(2);
    expect(idleCalls).toBe(0);
  });

  test('an incomplete runtime projection never settles the current generation', async () => {
    let reads = 0;
    let idleCalls = 0;
    const result = await reconcileConversationTurnAfterStreamTerminal(
      conversationId,
      () => reads < 2,
      () => {
        idleCalls += 1;
      },
      [0],
      async () => {
        reads += 1;
        return unknownConversation;
      },
      5,
      true
    );

    expect(result).toBe(false);
    expect(reads).toBe(2);
    expect(idleCalls).toBe(0);
  });

  test('an accepted replay opens only after a running GET and settles on idle', async () => {
    const snapshots = [busyConversation, idleConversation];
    let processingCalls = 0;
    let idleCalls = 0;

    const result = await reconcileConversationTurnAfterAcceptedReplay(
      conversationId,
      () => true,
      () => {
        processingCalls += 1;
      },
      () => {
        idleCalls += 1;
      },
      [0, 0],
      async () => snapshots.shift() ?? idleConversation,
      5,
      false
    );

    expect(result).toBe(true);
    expect(processingCalls).toBe(1);
    expect(idleCalls).toBe(1);
  });
});
