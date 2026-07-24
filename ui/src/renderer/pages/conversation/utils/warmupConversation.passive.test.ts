import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId, parseMessageId } from '@/common/types/ids';
import {
  shouldWarmupConversationOnPassiveMount,
  warmupConversationForPassiveMount,
} from './warmupConversation';

const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000061');
const activeTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000062');

const snapshot = (
  status: 'pending' | 'running' | 'finished',
  isProcessing = false
): TChatConversation =>
  ({
    id: conversationId,
    status,
    runtime: {
      state: isProcessing ? 'running' : 'idle',
      can_send_message: !isProcessing,
      has_runtime: isProcessing,
      runtime_status: status,
      is_processing: isProcessing,
      pending_confirmations: 0,
    },
  }) as TChatConversation;

describe('passive conversation warmup authority', () => {
  test('Finished remount is GET-only and never invokes the warmup POST', async () => {
    let warmupCalls = 0;

    const warmed = await warmupConversationForPassiveMount(conversationId, {
      getConversation: async () => snapshot('finished'),
      warmup: async () => {
        warmupCalls += 1;
      },
    });

    expect(warmed).toBe(false);
    expect(warmupCalls).toBe(0);
  });

  test('Running remount cannot issue a competing warmup POST', async () => {
    let warmupCalls = 0;

    const warmed = await warmupConversationForPassiveMount(conversationId, {
      getConversation: async () => snapshot('running', true),
      warmup: async () => {
        warmupCalls += 1;
      },
    });

    expect(warmed).toBe(false);
    expect(warmupCalls).toBe(0);
  });

  test('only exact Pending plus idle may be prepared passively', async () => {
    let warmupCalls = 0;

    expect(shouldWarmupConversationOnPassiveMount(null)).toBe(false);
    expect(shouldWarmupConversationOnPassiveMount(snapshot('pending', true))).toBe(false);
    expect(shouldWarmupConversationOnPassiveMount(snapshot('pending'))).toBe(true);

    const warmed = await warmupConversationForPassiveMount(conversationId, {
      getConversation: async () => snapshot('pending'),
      warmup: async (receivedId) => {
        expect(receivedId).toBe(conversationId);
        warmupCalls += 1;
      },
    });

    expect(warmed).toBe(true);
    expect(warmupCalls).toBe(1);
  });

  test('Pending with a contradictory active owner fails closed', async () => {
    let warmupCalls = 0;
    const pendingWithOwner = snapshot('pending');
    pendingWithOwner.runtime = {
      ...pendingWithOwner.runtime!,
      active_turn_id: activeTurnId,
    };

    const warmed = await warmupConversationForPassiveMount(conversationId, {
      getConversation: async () => pendingWithOwner,
      warmup: async () => {
        warmupCalls += 1;
      },
    });

    expect(warmed).toBe(false);
    expect(warmupCalls).toBe(0);
  });
});
