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
