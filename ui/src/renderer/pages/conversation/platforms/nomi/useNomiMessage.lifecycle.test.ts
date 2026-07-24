import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { parseMessageId } from '@/common/types/ids';
import {
  classifyAuthoritativeTurnStart,
  resolveVerifiedAuthoritativeTurnStart,
} from '../authoritativeTurnLifecyclePolicy';
import {
  getNomiHydrationLifecycleFence,
  shouldApplyNomiStreamEventToTurn,
} from './nomiLifecycleFence';

const COMPLETED_TURN_ID = parseMessageId('0190f5fe-7c00-7a00-8000-000000000041');
const ACCEPTED_TURN_ID = parseMessageId('0190f5fe-7c00-7a00-8000-000000000042');

describe('useNomiMessage terminal lifecycle fence', () => {
  test('the hook wires the idle snapshot fence before hydration can race a late start', () => {
    const source = readFileSync(new URL('./useNomiMessage.ts', import.meta.url), 'utf8');
    const resetIndex = source.indexOf("dispatchTurn({ type: 'reset' });");
    const pendingFenceIndex = source.indexOf(
      'const pendingHydrationFence = getNomiHydrationLifecycleFence(false);'
    );
    const hydrateRequestIndex = source.indexOf('void getConversationOrNull(conversation_id).then');

    expect(resetIndex).toBeGreaterThanOrEqual(0);
    expect(pendingFenceIndex).toBeGreaterThan(resetIndex);
    expect(hydrateRequestIndex).toBeGreaterThan(pendingFenceIndex);
    expect(source.includes('shouldApplyNomiStreamEventToTurn({')).toBe(true);
    expect(source.includes("dispatchTurn({ type: 'hydrate', isRunning, settleIdle: true });")).toBe(
      true
    );
  });

  test('fresh idle hydration keeps late prior-turn stream events projection-only', () => {
    const fence = getNomiHydrationLifecycleFence(false);

    expect(
      shouldApplyNomiStreamEventToTurn({
        eventTurnId: COMPLETED_TURN_ID,
        activeTurnId: null,
        turnClosed: fence.turnClosed,
        awaitingBackendTurn: false,
      })
    ).toBe(false);
  });

  test('idle hydration verifies an unannounced start and only accepts a live runtime', () => {
    const fence = getNomiHydrationLifecycleFence(false);
    const startAction = classifyAuthoritativeTurnStart({
      turnId: ACCEPTED_TURN_ID,
      cancelledTurnIds: new Set(),
      rejectUnannouncedStart: false,
      awaitingBackendTurn: false,
      verifyUnannouncedStartRuntime: fence.verifyUnannouncedStartRuntime,
    });

    expect(startAction).toBe('verify_runtime');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: ACCEPTED_TURN_ID,
        runtimeIsProcessing: false,
      })
    ).toBe('ignore');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: ACCEPTED_TURN_ID,
        runtimeIsProcessing: true,
        eventActiveTurnId: ACCEPTED_TURN_ID,
        runtimeActiveTurnId: ACCEPTED_TURN_ID,
      })
    ).toBe('accept');
  });

  test('running hydration keeps processing visible but rejects a stale active turn id', () => {
    const fence = getNomiHydrationLifecycleFence(true);

    expect(fence.turnClosed).toBe(false);
    expect(fence.verifyUnannouncedStartRuntime).toBe(true);
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: COMPLETED_TURN_ID,
        runtimeIsProcessing: true,
        eventActiveTurnId: COMPLETED_TURN_ID,
        runtimeActiveTurnId: ACCEPTED_TURN_ID,
      })
    ).toBe('ignore');
  });

  test('an accepted new turn applies matching stream events but fences late foreign output', () => {
    const openFence = getNomiHydrationLifecycleFence(true);

    expect(
      shouldApplyNomiStreamEventToTurn({
        eventTurnId: ACCEPTED_TURN_ID,
        activeTurnId: ACCEPTED_TURN_ID,
        turnClosed: openFence.turnClosed,
        awaitingBackendTurn: false,
      })
    ).toBe(true);
    expect(
      shouldApplyNomiStreamEventToTurn({
        eventTurnId: COMPLETED_TURN_ID,
        activeTurnId: ACCEPTED_TURN_ID,
        turnClosed: openFence.turnClosed,
        awaitingBackendTurn: false,
      })
    ).toBe(false);
  });

  test('a local submit explicitly opens the fence while awaiting backend acceptance', () => {
    expect(
      shouldApplyNomiStreamEventToTurn({
        activeTurnId: null,
        turnClosed: false,
        awaitingBackendTurn: true,
      })
    ).toBe(true);
  });

  test('an old correlated frame cannot claim a local submit before turn.started', () => {
    expect(
      shouldApplyNomiStreamEventToTurn({
        eventTurnId: COMPLETED_TURN_ID,
        activeTurnId: null,
        turnClosed: false,
        awaitingBackendTurn: true,
      })
    ).toBe(false);
  });

  test('an uncorrelated frame cannot mutate an exact active turn', () => {
    expect(
      shouldApplyNomiStreamEventToTurn({
        activeTurnId: ACCEPTED_TURN_ID,
        turnClosed: false,
        awaitingBackendTurn: false,
      })
    ).toBe(false);
  });
});
