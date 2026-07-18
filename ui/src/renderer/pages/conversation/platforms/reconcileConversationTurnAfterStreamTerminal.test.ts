import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId } from '@/common/types/ids';
import {
  TERMINAL_RECONCILE_DELAYS_MS,
  reconcileConversationTurnAfterStreamTerminal,
  terminalReconcileDelayForAttempt,
} from './reconcileConversationTurnAfterStreamTerminal';

const conversationId = parseConversationId('conv_0190f5fe-7c00-7a00-8000-000000000041');
const idleConversation = { runtime: { is_processing: false } } as TChatConversation;
const busyConversation = { runtime: { is_processing: true } } as TChatConversation;

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
});
