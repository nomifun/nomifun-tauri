import { describe, expect, test } from 'bun:test';
import { parseMessageId } from '@/common/types/ids';
import {
  classifyAuthoritativeTurnStart,
  classifyAuthoritativeTurnCompletion,
  resolveVerifiedAuthoritativeTurnStart,
  shouldAcceptAuthoritativeTurnStart,
} from './authoritativeTurnLifecyclePolicy';

const oldTurnId = parseMessageId('msg_0190f5fe-7c00-7a00-8000-000000000011');
const newTurnId = parseMessageId('msg_0190f5fe-7c00-7a00-8000-000000000012');

describe('authoritative turn lifecycle policy', () => {
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

  test('known-root stop admits a different authoritative turn while stop confirmation is pending', () => {
    const pendingStop = {
      cancelledTurnIds: new Set([oldTurnId]),
      rejectUnannouncedStart: true,
      awaitingBackendTurn: false,
      verifyUnannouncedStartRuntime: false,
    };

    expect(classifyAuthoritativeTurnStart({ ...pendingStop, turnId: oldTurnId })).toBe('ignore');
    expect(classifyAuthoritativeTurnStart({ ...pendingStop, turnId: newTurnId })).toBe('accept');
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
    expect(resolveVerifiedAuthoritativeTurnStart({ runtimeIsProcessing: false })).toBe('ignore');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        runtimeIsProcessing: true,
        eventProcessingStartedAt: 100,
        runtimeProcessingStartedAt: 100,
      })
    ).toBe('accept');
    expect(
      resolveVerifiedAuthoritativeTurnStart({
        runtimeIsProcessing: true,
        eventProcessingStartedAt: 100,
        runtimeProcessingStartedAt: 200,
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
