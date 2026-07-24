import { describe, expect, test } from 'bun:test';
import { parseMessageId } from '@/common/types/ids';
import {
  classifyAuthoritativeTurnStart,
  classifyAuthoritativeTurnCompletion,
  isAuthoritativeCompletionRuntimeIdle,
  resolveVerifiedAuthoritativeTurnStart,
  shouldAcceptAuthoritativeTurnStart,
} from './authoritativeTurnLifecyclePolicy';
import {
  getAuthoritativeHydrationFence,
  shouldAcceptAuthoritativeStreamActivity,
} from './useAuthoritativeTurnLifecycle';

const oldTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000011');
const newTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000012');

describe('authoritative turn lifecycle policy', () => {
  test('completion runtime must explicitly be idle and have no active owner', () => {
    expect(
      isAuthoritativeCompletionRuntimeIdle({
        is_processing: false,
      })
    ).toBe(true);
    expect(
      isAuthoritativeCompletionRuntimeIdle({
        is_processing: true,
      })
    ).toBe(false);
    expect(
      isAuthoritativeCompletionRuntimeIdle({
        is_processing: false,
        active_turn_id: newTurnId,
      })
    ).toBe(false);
  });

  test('pending and idle hydration fence late stream activity and verify unannounced starts', () => {
    const fence = getAuthoritativeHydrationFence(false);

    expect(
      shouldAcceptAuthoritativeStreamActivity({
        closed: fence.closed,
        awaitingBackendTurn: false,
      })
    ).toBe(false);
    expect(
      classifyAuthoritativeTurnStart({
        turnId: oldTurnId,
        cancelledTurnIds: new Set(),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: fence.verifyUnannouncedStartRuntime,
      })
    ).toBe('verify_runtime');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: oldTurnId,
        runtimeIsProcessing: false,
      })
    ).toBe('ignore');
  });

  test('an explicit local submit opens activity while the backend turn is pending', () => {
    expect(
      shouldAcceptAuthoritativeStreamActivity({
        closed: false,
        awaitingBackendTurn: true,
      })
    ).toBe(true);
  });

  test('a correlated prior-turn stream cannot cross an awaiting local submit fence', () => {
    expect(
      shouldAcceptAuthoritativeStreamActivity({
        closed: false,
        awaitingBackendTurn: true,
        activeTurnId: null,
        eventTurnId: oldTurnId,
      })
    ).toBe(false);
    expect(
      classifyAuthoritativeTurnStart({
        turnId: oldTurnId,
        activeTurnId: null,
        cancelledTurnIds: new Set(),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: true,
        verifyUnannouncedStartRuntime: true,
      })
    ).toBe('verify_runtime');
  });

  test('stop without an observed root rejects a late unannounced start', () => {
    expect(
      classifyAuthoritativeTurnStart({
        turnId: oldTurnId,
        cancelledTurnIds: new Set(),
        rejectUnannouncedStart: true,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: true,
      })
    ).toBe('ignore');
  });

  test('known stopped turn ids remain rejected after stop is confirmed', () => {
    expect(
      shouldAcceptAuthoritativeTurnStart({
        turnId: oldTurnId,
        cancelledTurnIds: new Set([oldTurnId]),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: false,
      })
    ).toBe(false);
    expect(
      shouldAcceptAuthoritativeTurnStart({
        turnId: newTurnId,
        cancelledTurnIds: new Set([oldTurnId]),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: false,
      })
    ).toBe(true);
  });

  test('known-root stop requires exact runtime proof for a different turn while confirmation is pending', () => {
    const pendingStop = {
      cancelledTurnIds: new Set([oldTurnId]),
      rejectUnannouncedStart: true,
      awaitingBackendTurn: false,
      verifyUnannouncedStartRuntime: false,
    };

    expect(classifyAuthoritativeTurnStart({ ...pendingStop, turnId: oldTurnId })).toBe('ignore');
    expect(classifyAuthoritativeTurnStart({ ...pendingStop, turnId: newTurnId })).toBe('verify_runtime');
  });

  test('unknown-root stop confirmation verifies late starts without blocking genuine external turns forever', () => {
    const input = {
      turnId: oldTurnId,
      cancelledTurnIds: new Set<ReturnType<typeof parseMessageId>>(),
      rejectUnannouncedStart: false,
      awaitingBackendTurn: false,
      verifyUnannouncedStartRuntime: true,
    };

    expect(classifyAuthoritativeTurnStart(input)).toBe('verify_runtime');
    // An idle runtime keeps the verification boundary armed, so the delayed old
    // start is ignored. A later real cron/remote start gets the same check and
    // is accepted by the lifecycle only when GET reports processing=true.
    expect(classifyAuthoritativeTurnStart({ ...input, turnId: newTurnId })).toBe('verify_runtime');
    expect(classifyAuthoritativeTurnStart({ ...input, awaitingBackendTurn: true })).toBe('verify_runtime');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: oldTurnId,
        runtimeIsProcessing: false,
      })
    ).toBe('ignore');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: oldTurnId,
        runtimeIsProcessing: true,
      })
    ).toBe('ignore');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: oldTurnId,
        runtimeIsProcessing: true,
        eventActiveTurnId: oldTurnId,
        runtimeActiveTurnId: oldTurnId,
      })
    ).toBe('accept');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: oldTurnId,
        runtimeIsProcessing: true,
        eventActiveTurnId: oldTurnId,
        runtimeActiveTurnId: newTurnId,
      })
    ).toBe('ignore');
  });

  test('an active root cannot be replaced by a delayed conflicting start event', () => {
    expect(
      classifyAuthoritativeTurnStart({
        turnId: oldTurnId,
        activeTurnId: newTurnId,
        cancelledTurnIds: new Set(),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: false,
      })
    ).toBe('verify_runtime');
    expect(
      classifyAuthoritativeTurnStart({
        turnId: newTurnId,
        activeTurnId: newTurnId,
        cancelledTurnIds: new Set(),
        rejectUnannouncedStart: false,
        awaitingBackendTurn: false,
        verifyUnannouncedStartRuntime: false,
      })
    ).toBe('ignore');

    expect(
      resolveVerifiedAuthoritativeTurnStart({
        turnId: oldTurnId,
        runtimeIsProcessing: true,
        eventActiveTurnId: oldTurnId,
        runtimeActiveTurnId: newTurnId,
      })
    ).toBe('ignore');
  });

  test('old completion is ignored while a new submit awaits acceptance', () => {
    expect(
      classifyAuthoritativeTurnCompletion({
        rootTurnId: null,
        completedTurnId: oldTurnId,
        awaitingBackendTurn: true,
      })
    ).toBe('ignore');
    expect(
      classifyAuthoritativeTurnCompletion({
        rootTurnId: null,
        completedTurnId: undefined,
        awaitingBackendTurn: true,
      })
    ).toBe('ignore');
  });

  test('a null-turn stop completion is runtime-reconciled only when no new submit is awaiting', () => {
    expect(
      classifyAuthoritativeTurnCompletion({
        rootTurnId: null,
        completedTurnId: undefined,
        awaitingBackendTurn: false,
      })
    ).toBe('reconcile_runtime');
  });

  test('completion is exact when correlated and runtime-reconciled when start was missed after acceptance', () => {
    expect(
      classifyAuthoritativeTurnCompletion({
        rootTurnId: newTurnId,
        completedTurnId: newTurnId,
        awaitingBackendTurn: false,
      })
    ).toBe('settle');
    expect(
      classifyAuthoritativeTurnCompletion({
        rootTurnId: newTurnId,
        completedTurnId: oldTurnId,
        awaitingBackendTurn: false,
      })
    ).toBe('ignore');
    expect(
      classifyAuthoritativeTurnCompletion({
        rootTurnId: null,
        completedTurnId: newTurnId,
        awaitingBackendTurn: false,
      })
    ).toBe('reconcile_runtime');
  });
});
