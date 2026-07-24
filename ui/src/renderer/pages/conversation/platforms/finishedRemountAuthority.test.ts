import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId, parseMessageId } from '@/common/types/ids';
import { warmupConversationForPassiveMount } from '../utils/warmupConversation';
import {
  classifyAuthoritativeTurnStart,
  resolveVerifiedAuthoritativeTurnStart,
} from './authoritativeTurnLifecyclePolicy';
import {
  getAuthoritativeHydrationFence,
  shouldAcceptAuthoritativeStreamActivity,
} from './useAuthoritativeTurnLifecycle';
import { shouldApplyAcpStreamEventToTurn } from './acp/useAcpMessage';
import {
  getNomiHydrationLifecycleFence,
  shouldApplyNomiStreamEventToTurn,
} from './nomi/nomiLifecycleFence';
import {
  IDLE_EXECUTION_GATE,
  reduceCommandQueueExecutionGate,
  shouldDispatchConversationCommandQueue,
} from './commandQueueExecutionGate';

const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000071');
const completedTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000072');

const finishedSnapshot = {
  id: conversationId,
  status: 'finished',
  runtime: {
    state: 'idle',
    can_send_message: true,
    has_runtime: false,
    runtime_status: 'finished',
    is_processing: false,
    pending_confirmations: 0,
  },
} as TChatConversation;

describe('Finished conversation remount authority', () => {
  test('late old started/stream/completed plus remount stays idle and sends no POST', async () => {
    let warmupPostCount = 0;
    let sendPostCount = 0;
    let busy = false;

    // Nomi/ACP passive mount first performs an authoritative GET. Finished is
    // a terminal snapshot and cannot trigger runtime preparation.
    expect(
      await warmupConversationForPassiveMount(conversationId, {
        getConversation: async () => finishedSnapshot,
        warmup: async () => {
          warmupPostCount += 1;
        },
      })
    ).toBe(false);

    const simpleFence = getAuthoritativeHydrationFence(false);
    const nomiFence = getNomiHydrationLifecycleFence(false);

    // Delayed old output cannot raise Remote/OpenClaw/Nanobot, Nomi, or ACP.
    expect(
      shouldAcceptAuthoritativeStreamActivity({
        closed: simpleFence.closed,
        awaitingBackendTurn: false,
        activeTurnId: null,
        eventTurnId: completedTurnId,
      })
    ).toBe(false);
    expect(
      shouldApplyNomiStreamEventToTurn({
        eventTurnId: completedTurnId,
        activeTurnId: null,
        turnClosed: nomiFence.turnClosed,
        awaitingBackendTurn: false,
      })
    ).toBe(false);
    expect(
      shouldApplyAcpStreamEventToTurn({
        eventTurnId: completedTurnId,
        activeTurnId: undefined,
        turnClosed: true,
        awaitingBackendTurn: false,
      })
    ).toBe(false);

    // A delayed old start remains behind exact active_turn_id verification.
    expect(
      classifyAuthoritativeTurnStart({
        turnId: completedTurnId,
        activeTurnId: null,
        cancelledTurnIds: new Set(),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: true,
      })
    ).toBe('verify_runtime');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: completedTurnId,
        runtimeIsProcessing: finishedSnapshot.runtime?.is_processing === true,
        eventActiveTurnId: completedTurnId,
        runtimeActiveTurnId: finishedSnapshot.runtime?.active_turn_id,
      })
    ).toBe('ignore');

    // The queue also refuses to correlate a raw old start/completion pair.
    // With no persisted user command, no execution callback is reachable.
    const gated = reduceCommandQueueExecutionGate(IDLE_EXECUTION_GATE, {
      type: 'turnStarted',
      turnId: completedTurnId,
    });
    expect(
      reduceCommandQueueExecutionGate(gated, {
        type: 'turnCompleted',
        turnId: completedTurnId,
        runtimeIsProcessing: false,
      })
    ).toBe(gated);
    expect(
      reduceCommandQueueExecutionGate(gated, {
        type: 'runtimeReconciled',
        purpose: 'completion',
        runtimeIsProcessing: false,
      })
    ).toBe(IDLE_EXECUTION_GATE);

    // The production queue dispatcher has no persisted user intent and remains
    // unreachable after the stale-event reconciliation.
    const shouldDispatch = shouldDispatchConversationCommandQueue({
      enabled: true,
      isHydrated: true,
      isPaused: false,
      isBusy: busy,
      gate: IDLE_EXECUTION_GATE,
      isInteractionLocked: false,
      itemCount: 0,
    });
    if (shouldDispatch) {
      sendPostCount += 1;
      busy = true;
    }
    expect(busy).toBe(false);
    expect(warmupPostCount).toBe(0);
    expect(sendPostCount).toBe(0);
  });
});
