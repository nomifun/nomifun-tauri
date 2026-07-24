import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./useConversationCommandQueue.ts', import.meta.url), 'utf8');

describe('conversation command queue runtime recovery', () => {
  test('bounds authoritative runtime reads so one hung request cannot strand the gate', () => {
    expect(source.includes('COMMAND_QUEUE_RUNTIME_QUERY_TIMEOUT_MS')).toBe(true);
    expect(source.includes('Promise.race([')).toBe(true);
    expect(source.includes('getConversationForCommandQueue(conversationKey)')).toBe(true);
  });

  test('reconciles waiting_start as a start acknowledgement on the later busy down-edge', () => {
    const reconcile = source.indexOf('const reconcileActiveExecution = useCallback');
    const dynamicPurpose = source.indexOf(
      "const purpose = gate.phase === 'waiting_start' ? 'start' : 'completion';",
      reconcile
    );
    const reduction = source.indexOf('purpose,', dynamicPurpose);
    const acceptedSendRetry = source.indexOf('void reconcileActiveExecution();', reduction);

    expect(reconcile >= 0).toBe(true);
    expect(dynamicPurpose > reconcile).toBe(true);
    expect(reduction > dynamicPurpose).toBe(true);
    expect(acceptedSendRetry > reduction).toBe(true);
  });

  test('keeps capped reconciliation retries alive until an authoritative read succeeds', () => {
    const recoveryEffect = source.indexOf('idle UI still owns a non-idle gate');
    const retryLoop = source.indexOf('while (!cancelled)', recoveryEffect);
    const cappedDelay = source.indexOf('getCommandQueueReconcileDelayMs(attempt)', retryLoop);
    const authoritativeRead = source.indexOf('await reconcileActiveExecution()', cappedDelay);
    const nextAttempt = source.indexOf('attempt += 1;', authoritativeRead);

    expect(recoveryEffect >= 0).toBe(true);
    expect(retryLoop > recoveryEffect).toBe(true);
    expect(cappedDelay > retryLoop).toBe(true);
    expect(authoritativeRead > cappedDelay).toBe(true);
    expect(nextAttempt > authoritativeRead).toBe(true);
  });

  test('invalidates a dequeued POST across stop, reset, deletion, and conversation changes', () => {
    const generationRef = source.indexOf('const executionGenerationRef = useRef(0);');
    const conversationScope = source.indexOf('executionConversationKeyRef.current = conversationKey;', generationRef);
    const reset = source.indexOf("(reason: 'stop' | 'external-reset') => {", conversationScope);
    const resetInvalidation = source.indexOf('executionGenerationRef.current += 1;', reset);
    const deletion = source.indexOf("'conversation.deleted'", conversationScope);
    const deletionInvalidation = source.indexOf('executionGenerationRef.current += 1;', deletion);
    const conversationEffect = source.indexOf('}, [conversationKey]);', conversationScope);

    expect(generationRef >= 0).toBe(true);
    expect(conversationScope > generationRef).toBe(true);
    expect(resetInvalidation > reset).toBe(true);
    expect(deletionInvalidation > deletion).toBe(true);
    expect(conversationEffect > conversationScope).toBe(true);
  });

  test('keeps the item persisted through POST and removes it only after acceptance', () => {
    const executionEffect = source.indexOf('const [nextCommand] = data.items;');
    const persistenceComment = source.indexOf('Keep the item durably queued while the request is in flight', executionEffect);
    const dispatch = source.indexOf('void Promise.resolve()', persistenceComment);
    const currentFence = source.indexOf('if (!isExecutionCurrent()) return;', dispatch);
    const execute = source.indexOf(
      'return onExecute(nextCommand, { isCurrent: isExecutionCurrent });',
      currentFence
    );
    const accepted = source.indexOf('.then(async (deliveryDisposition) => {', execute);
    const acceptedFence = source.indexOf('if (!isExecutionCurrent()) return;', accepted);
    const acceptedUpdater = source.indexOf('await updateState((state) =>', acceptedFence);
    const acceptedUpdaterFence = source.indexOf('isExecutionCurrent()', acceptedUpdater);
    const remove = source.indexOf('items: removeQueuedCommand(state.items, nextCommand.id)', acceptedFence);

    expect(executionEffect >= 0).toBe(true);
    expect(persistenceComment > executionEffect).toBe(true);
    expect(dispatch > persistenceComment).toBe(true);
    expect(currentFence > dispatch).toBe(true);
    expect(execute > currentFence).toBe(true);
    expect(accepted > execute).toBe(true);
    expect(acceptedFence > accepted).toBe(true);
    expect(acceptedUpdater > acceptedFence).toBe(true);
    expect(acceptedUpdaterFence > acceptedUpdater).toBe(true);
    expect(remove > acceptedUpdaterFence).toBe(true);
  });

  test('a completed replay releases the queue without waiting for a new turn', () => {
    const accepted = source.indexOf('.then(async (deliveryDisposition) => {');
    const replay = source.indexOf(
      "if (deliveryDisposition === 'replayed_completed')",
      accepted
    );
    const release = source.indexOf(
      'executionGateRef.current = IDLE_EXECUTION_GATE;',
      replay
    );
    const freshReconcile = source.indexOf('void reconcileActiveExecution();', release);

    expect(accepted >= 0).toBe(true);
    expect(replay > accepted).toBe(true);
    expect(release > replay).toBe(true);
    expect(freshReconcile > release).toBe(true);
  });

  test('stale execution outcomes cannot reconcile, restore, pause, or warn', () => {
    const execute = source.indexOf(
      'return onExecute(nextCommand, { isCurrent: isExecutionCurrent });'
    );
    const resolveFence = source.indexOf('if (!isExecutionCurrent()) return;', execute);
    const acceptedRemoval = source.indexOf('items: removeQueuedCommand(state.items, nextCommand.id)', resolveFence);
    const postRemovalFence = source.indexOf('if (!isExecutionCurrent()) return;', acceptedRemoval);
    const reconcile = source.indexOf('void reconcileActiveExecution();', postRemovalFence);
    const reject = source.indexOf('.catch((error) => {', reconcile);
    const rejectFence = source.indexOf('if (!isExecutionCurrent()', reject);
    const acceptedFence = source.indexOf("executionGateRef.current.phase !== 'waiting_start'", rejectFence);
    const restoreUpdater = source.indexOf('void updateState((state) =>', acceptedFence);
    const restoreUpdaterFence = source.indexOf('isExecutionCurrent()', restoreUpdater);
    const restore = source.indexOf('restoreQueuedCommand(state.items, nextCommand)', restoreUpdaterFence);
    const warning = source.indexOf('Message.warning(', restore);

    expect(execute >= 0).toBe(true);
    expect(resolveFence > execute).toBe(true);
    expect(acceptedRemoval > resolveFence).toBe(true);
    expect(postRemovalFence > acceptedRemoval).toBe(true);
    expect(reconcile > postRemovalFence).toBe(true);
    expect(reject > reconcile).toBe(true);
    expect(rejectFence > reject).toBe(true);
    expect(acceptedFence > rejectFence).toBe(true);
    expect(restoreUpdater > acceptedFence).toBe(true);
    expect(restoreUpdaterFence > restoreUpdater).toBe(true);
    expect(restore > restoreUpdaterFence).toBe(true);
    expect(warning > restore).toBe(true);
  });
});
