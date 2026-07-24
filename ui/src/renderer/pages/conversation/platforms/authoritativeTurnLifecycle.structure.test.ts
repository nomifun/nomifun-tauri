import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const sharedLifecycle = readFileSync(new URL('./useAuthoritativeTurnLifecycle.ts', import.meta.url), 'utf8');
const simpleSendBoxes = [
  './nanobot/NanobotSendBox.tsx',
  './remote/RemoteSendBox.tsx',
  './openclaw/OpenClawSendBox.tsx',
].map((path) => readFileSync(new URL(path, import.meta.url), 'utf8'));
const statefulLifecycles = ['./nomi/useNomiMessage.ts', './acp/useAcpMessage.ts'].map((path) =>
  readFileSync(new URL(path, import.meta.url), 'utf8')
);

describe('authoritative turn lifecycle wiring', () => {
  test('closes simple-platform lifecycle before hydration and applies the fresh snapshot', () => {
    const conversationReset = sharedLifecycle.indexOf('}, [conversationId]);');
    const closeBeforeHydration = sharedLifecycle.lastIndexOf(
      'closedRef.current = true;',
      conversationReset
    );
    const verifyBeforeHydration = sharedLifecycle.lastIndexOf(
      'verifyUnannouncedStartRuntimeRef.current = true;',
      conversationReset
    );

    expect(closeBeforeHydration).toBeGreaterThan(-1);
    expect(verifyBeforeHydration).toBeGreaterThan(closeBeforeHydration);
    for (const source of simpleSendBoxes) {
      expect(source.includes('hydrateAuthoritativeRuntime(isRunning);')).toBe(true);
    }
  });

  test('closes ACP lifecycle before its hydration request and requires exact stream correlation', () => {
    const acpSource = statefulLifecycles[1];
    const hydration = acpSource.indexOf('// Reset state when conversation changes');
    const closed = acpSource.indexOf('turnClosedRef.current = true;', hydration);
    const verify = acpSource.indexOf('verifyUnannouncedStartRuntimeRef.current = true;', hydration);
    const request = acpSource.indexOf('void getConversationOrNull(conversation_id)', hydration);

    expect(closed).toBeGreaterThan(hydration);
    expect(verify).toBeGreaterThan(hydration);
    expect(request).toBeGreaterThan(closed);
    expect(request).toBeGreaterThan(verify);
    expect(acpSource.includes('shouldApplyAcpStreamEventToTurn({')).toBe(true);
  });

  test('invalidates pending stop continuations at the authoritative completion boundary', () => {
    expect(sharedLifecycle.includes('turnCompletionGenerationRef.current += 1;')).toBe(true);
    expect(sharedLifecycle.includes('getTurnCompletionGeneration')).toBe(true);
    for (const source of statefulLifecycles) {
      expect(source.includes('turnCompletionGenerationRef.current += 1;')).toBe(true);
      expect(source.includes('getTurnCompletionGeneration')).toBe(true);
    }
  });

  test('does not let a stale hydration snapshot resurrect a completed turn', () => {
    for (const source of simpleSendBoxes) {
      expect(source.includes('const hydrationGeneration = getTurnLifecycleGeneration();')).toBe(true);
      expect(source.includes('getTurnLifecycleGeneration() !== hydrationGeneration')).toBe(true);
    }
    for (const source of statefulLifecycles) {
      expect(source.includes('const hydrationGeneration = turnLifecycleGenerationRef.current;')).toBe(true);
      expect(source.includes('turnLifecycleGenerationRef.current !== hydrationGeneration')).toBe(true);
    }
  });

  test('keeps automatic queue delivery closed when runtime authority is incomplete', () => {
    for (const source of [...simpleSendBoxes, ...statefulLifecycles]) {
      expect(source.includes('getConversationRuntimeAuthority(res)')).toBe(true);
      expect(source.includes("setHasHydratedRunningState(runtimeAuthority !== 'unknown')")).toBe(
        true
      );
    }

    const acpSource = statefulLifecycles[1];
    const hydrationFailure = acpSource.indexOf(
      '// A failed authority read is not an idle snapshot.'
    );
    expect(hydrationFailure).toBeGreaterThan(-1);
    expect(acpSource.indexOf('setHasHydratedRunningState(false);', hydrationFailure)).toBeGreaterThan(
      hydrationFailure
    );
  });

  test('generation-fences uncorrelated completion reconciliation against null-root ABA', () => {
    for (const source of statefulLifecycles) {
      const rootSnapshot = source.indexOf('const observedRootTurnId = rootTurnId;');
      const awaitingSnapshot = source.indexOf(
        'const observedAwaitingBackendTurn = awaitingBackendTurn;',
        rootSnapshot
      );
      const generationSnapshot = source.indexOf(
        'const generation = turnLifecycleGenerationRef.current;',
        awaitingSnapshot
      );
      const reconcile = source.indexOf(
        'reconcileConversationTurnAfterStreamTerminal(',
        generationSnapshot
      );
      const generationCheck = source.indexOf(
        'turnLifecycleGenerationRef.current === generation',
        reconcile
      );
      const rootCheck = source.indexOf(
        'rootTurnIdRef.current === observedRootTurnId',
        generationCheck
      );
      const awaitingCheck = source.indexOf(
        'awaitingBackendTurnRef.current === observedAwaitingBackendTurn',
        rootCheck
      );
      const settle = source.indexOf('settleCompletedTurn', awaitingCheck);

      expect(rootSnapshot >= 0).toBe(true);
      expect(awaitingSnapshot > rootSnapshot).toBe(true);
      expect(generationSnapshot > awaitingSnapshot).toBe(true);
      expect(reconcile > generationSnapshot).toBe(true);
      expect(generationCheck > reconcile).toBe(true);
      expect(rootCheck > generationCheck).toBe(true);
      expect(awaitingCheck > rootCheck).toBe(true);
      expect(settle > awaitingCheck).toBe(true);
    }
  });

  test('reconciles again after stop-failure restoration invalidates an in-flight null-turn GET', () => {
    const sharedRestore = sharedLifecycle.indexOf('const restoreAfterStopFailure = useCallback');
    expect(sharedLifecycle.indexOf('reconcileGeneration(generationRef.current);', sharedRestore)).toBeGreaterThan(
      sharedRestore
    );

    for (const source of statefulLifecycles) {
      const restore = source.indexOf('const restoreRunningAfterStopFailure = useCallback');
      const generation = source.indexOf('const generation = turnLifecycleGenerationRef.current;', restore);
      const reconcile = source.indexOf('reconcileConversationTurnAfterStreamTerminal(', generation);
      expect(restore >= 0).toBe(true);
      expect(generation > restore).toBe(true);
      expect(reconcile > generation).toBe(true);
    }
  });
});
