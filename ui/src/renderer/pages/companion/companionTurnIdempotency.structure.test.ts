import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

describe('companion turn idempotency wiring', () => {
  test('persists before POST, forwards the stable key, and consumes only after acceptance', () => {
    const submit = source.indexOf('const submitTurn = useCallback');
    const persist = source.indexOf('persistCompanionTurnDelivery(', submit);
    const dispatch = source.indexOf('return deliverTurn(delivery, storageKey);', persist);

    const deliver = source.indexOf('const deliverTurn = useCallback');
    const claim = source.indexOf('claimCompanionTurnDelivery(storageKey)', deliver);
    const post = source.indexOf('await ipcBridge.conversation.sendMessage.invoke({', claim);
    const headerKey = source.indexOf('idempotency_key,', post);
    const boundConversation = source.indexOf(
      'const { conversation_id, input: text, files, idempotency_key } = delivery;',
      deliver
    );
    const accepted = source.indexOf(
      'completeCompanionTurnDelivery(',
      headerKey
    );

    expect(persist).toBeGreaterThan(submit);
    expect(dispatch).toBeGreaterThan(persist);
    expect(claim).toBeGreaterThan(deliver);
    expect(boundConversation).toBeGreaterThan(deliver);
    expect(boundConversation).toBeLessThan(post);
    expect(post).toBeGreaterThan(claim);
    expect(headerKey).toBeGreaterThan(post);
    expect(accepted).toBeGreaterThan(headerKey);
  });

  test('replays only against the exact active Conversation and never ensures a successor', () => {
    const retry = source.indexOf('const retryPendingTurn = useCallback');
    const read = source.indexOf('readCompanionTurnDelivery(sessionStorage, storageKey)', retry);
    const activeRead = source.indexOf(
      'ipcBridge.companion.getCompanionSession.invoke({',
      read
    );
    const exactMatch = source.indexOf(
      'active.conversation_id !== delivery.conversation_id',
      activeRead
    );
    const authority = source.indexOf(
      'getConversationOrNull(delivery.conversation_id)',
      exactMatch
    );
    const finishedFence = source.indexOf(
      "conversation.status !== 'pending' && conversation.status !== 'running'",
      authority
    );
    const pendingInitialOnly = source.indexOf(
      "conversation.status === 'pending'",
      finishedFence
    );
    const replay = source.indexOf('await deliverTurn(', pendingInitialOnly);
    const recoveryFence = source.indexOf(
      'await deliverTurn(delivery, storageKey, true, true);',
      pendingInitialOnly
    );
    const recoveryEffect = source.indexOf('void retryPendingTurn();', replay);
    const retrySource = source.slice(retry, recoveryEffect);

    expect(retry).toBeGreaterThan(-1);
    expect(read).toBeGreaterThan(retry);
    expect(activeRead).toBeGreaterThan(read);
    expect(exactMatch).toBeGreaterThan(activeRead);
    expect(authority).toBeGreaterThan(exactMatch);
    expect(finishedFence).toBeGreaterThan(authority);
    expect(pendingInitialOnly).toBeGreaterThan(finishedFence);
    expect(replay).toBeGreaterThan(pendingInitialOnly);
    expect(recoveryFence).toBe(replay);
    expect(recoveryEffect).toBeGreaterThan(replay);
    expect(retrySource.includes('ensureThread()')).toBe(false);
  });
});
