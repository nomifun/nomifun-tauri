import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (relativePath: string): string =>
  readFileSync(new URL(relativePath, import.meta.url), 'utf8');

const queueSource = readSource('./useConversationCommandQueue.ts');
const acpSource = readSource('./acp/AcpSendBox.tsx');
const remoteSource = readSource('./remote/RemoteSendBox.tsx');
const nanobotSource = readSource('./nanobot/NanobotSendBox.tsx');
const openClawSource = readSource('./openclaw/OpenClawSendBox.tsx');
const nomiSource = readSource('./nomi/NomiSendBox.tsx');
const platformSources = [
  acpSource,
  remoteSource,
  nanobotSource,
  openClawSource,
  nomiSource,
];

describe('conversation send idempotency wiring', () => {
  test('uses the persisted UUIDv7 queue item id as the send key', () => {
    expect(queueSource.includes("import { uuidv7 } from '@/common/utils';")).toBe(true);
    expect(queueSource.includes('id: uuidv7(),')).toBe(true);

    for (const source of platformSources) {
      expect(source.includes('idempotency_key: id,')).toBe(true);
    }
  });

  test('uses UUIDv7 for every default direct-send id before forwarding it as the header key', () => {
    for (const source of platformSources) {
      expect(source.includes("import { uuid, uuidv7 } from '@/common/utils';")).toBe(true);

      const defaultId = source.indexOf('id = uuidv7(),');
      const forwardedId = source.indexOf('idempotency_key: id,', defaultId);

      expect(defaultId).toBeGreaterThan(-1);
      expect(forwardedId).toBeGreaterThan(defaultId);
    }
  });

  test('keeps queued work persisted until acceptance and retains the same id on failure', () => {
    const dispatch = queueSource.indexOf('return onExecute(nextCommand, { isCurrent: isExecutionCurrent });');
    const acceptedRemoval = queueSource.indexOf(
      'items: removeQueuedCommand(state.items, nextCommand.id)',
      dispatch
    );
    const failure = queueSource.indexOf('.catch((error) => {', acceptedRemoval);
    const restoration = queueSource.indexOf(
      'items: restoreQueuedCommand(state.items, nextCommand)',
      failure
    );

    expect(dispatch >= 0).toBe(true);
    expect(acceptedRemoval > dispatch).toBe(true);
    expect(failure > acceptedRemoval).toBe(true);
    expect(restoration > failure).toBe(true);
  });

  test('keeps durable queue replays closed and opens only a fresh accepted delivery', () => {
    const queueCallers = [
      {
        source: nomiSource,
        guardedDirectOpen: 'if (!deferLocalTurnUntilFresh) setWaitingResponse(true);',
        deferredFreshOpen: 'if (deferLocalTurnUntilFresh) {',
      },
      {
        source: acpSource,
        guardedDirectOpen: 'if (!deferLocalTurnUntilFresh) setAiProcessing(true);',
        deferredFreshOpen: 'if (deferLocalTurnUntilFresh) {',
      },
      {
        source: remoteSource,
        guardedDirectOpen: 'if (!deferLocalTurnUntilFresh) {',
        deferredFreshOpen: 'if (deferLocalTurnUntilFresh) {',
      },
      {
        source: nanobotSource,
        guardedDirectOpen: 'if (!deferLocalTurnUntilFresh) {',
        deferredFreshOpen: 'if (deferLocalTurnUntilFresh) {',
      },
      {
        source: openClawSource,
        guardedDirectOpen: 'if (!deferLocalTurnUntilFresh) {',
        deferredFreshOpen: 'if (deferLocalTurnUntilFresh) {',
      },
    ];

    for (const { source, guardedDirectOpen, deferredFreshOpen } of queueCallers) {
      const execute = source.indexOf('const executeCommand = useCallback(');
      const durableMode = source.indexOf(
        'deferLocalTurnUntilFresh = execution !== undefined',
        execute
      );
      const directOpen = source.indexOf(guardedDirectOpen, durableMode);
      const post = source.indexOf('sendMessage.invoke({', directOpen);
      const classification = source.indexOf(
        'const disposition = classifyPublicMessageDelivery(',
        post
      );
      const fresh = source.indexOf("if (disposition === 'fresh') {", classification);
      const freshOpen = source.indexOf(deferredFreshOpen, fresh);
      const accepted = source.indexOf('mark', freshOpen);
      const replayBranch = source.indexOf('} else {', accepted);
      const reconcile = source.indexOf('reconcilePublicDeliveryReplay(', replayBranch);

      expect(execute >= 0).toBe(true);
      expect(durableMode > execute).toBe(true);
      expect(directOpen > durableMode).toBe(true);
      expect(post > directOpen).toBe(true);
      expect(fresh > post).toBe(true);
      expect(freshOpen > fresh).toBe(true);
      expect(accepted > freshOpen).toBe(true);
      expect(replayBranch > accepted).toBe(true);
      expect(reconcile > replayBranch).toBe(true);
    }
  });

  test('authorizes exact initial payloads and marks only those POSTs initial-only', () => {
    const initialConsumers = [
      readSource('./acp/useAcpInitialMessage.ts'),
      readSource('./remote/RemoteSendBox.tsx'),
      readSource('./nanobot/NanobotSendBox.tsx'),
      readSource('./openclaw/OpenClawSendBox.tsx'),
      readSource('./nomi/NomiSendBox.tsx'),
    ];

    for (const source of initialConsumers) {
      const authority = source.indexOf('readAuthorizedInitialMessageDelivery(');
      const sendKey = source.indexOf('idempotency_key', authority);
      const initialOnly = source.indexOf(
        source === nomiSource ? 'initialOnly: true' : 'initial_only: true,',
        sendKey
      );
      const accepted = source.indexOf(
        'completeInitialMessageDelivery(sessionStorage, storageKey, idempotency_key)',
        initialOnly
      );

      expect(authority >= 0).toBe(true);
      expect(sendKey > authority).toBe(true);
      expect(initialOnly > sendKey).toBe(true);
      expect(accepted > initialOnly).toBe(true);
    }
  });

  test('keeps explicit StarOffice clicks legal while remount recovery fails closed', () => {
    const deliveryFunction = openClawSource.indexOf('const deliverStarOfficeRequest = useCallback(');
    const claim = openClawSource.indexOf(
      'if (!claimInitialMessageDelivery(storageKey)) return;',
      deliveryFunction
    );
    const post = openClawSource.indexOf(
      'const result = await ipcBridge.openclawConversation.sendMessage.invoke({',
      claim
    );
    const forwardedKey = openClawSource.indexOf('idempotency_key,', post);
    const initialMode = openClawSource.indexOf(
      'initial_only: initialOnly,',
      forwardedKey
    );
    const acceptedRemoval = openClawSource.indexOf(
      'completeInitialMessageDelivery(sessionStorage, storageKey, idempotency_key);',
      initialMode
    );
    const classification = openClawSource.indexOf(
      'const disposition = classifyPublicMessageDelivery(result);',
      acceptedRemoval
    );
    const fresh = openClawSource.indexOf("if (disposition === 'fresh') {", classification);
    const freshOpen = openClawSource.indexOf('beginLocalTurn();', fresh);

    const explicitHandler = openClawSource.indexOf("'staroffice.install.request'");
    const persistedBeforeDispatch = openClawSource.indexOf(
      'const delivery = persistInitialMessageDelivery(',
      explicitHandler
    );
    const explicitDispatch = openClawSource.indexOf(
      'void deliverStarOfficeRequest(delivery, storageKey);',
      persistedBeforeDispatch
    );
    const remountRead = openClawSource.indexOf(
      'const pending = readInitialMessageDelivery(sessionStorage, storageKey);',
      explicitDispatch
    );
    const terminalFence = openClawSource.indexOf(
      "conversation.status !== 'pending' && conversation.status !== 'running'",
      remountRead
    );
    const quarantine = openClawSource.indexOf(
      'quarantineInitialMessageDelivery(',
      terminalFence
    );
    const runningBranch = openClawSource.indexOf(
      "if (conversation.status === 'running') {",
      quarantine
    );
    const runningReplay = openClawSource.indexOf(
      'void deliverStarOfficeRequest(pending, storageKey, true);',
      runningBranch
    );
    const pendingAuthority = openClawSource.indexOf(
      'const authorized = await readAuthorizedInitialMessageDelivery(',
      runningReplay
    );
    const pendingInitialOnly = openClawSource.indexOf(
      'void deliverStarOfficeRequest(authorized, storageKey, true);',
      pendingAuthority
    );

    expect(deliveryFunction >= 0).toBe(true);
    expect(claim > deliveryFunction).toBe(true);
    expect(post > claim).toBe(true);
    expect(forwardedKey > post).toBe(true);
    expect(initialMode > forwardedKey).toBe(true);
    expect(acceptedRemoval > initialMode).toBe(true);
    expect(fresh > acceptedRemoval).toBe(true);
    expect(freshOpen > fresh).toBe(true);
    expect(openClawSource.slice(deliveryFunction, post).includes('beginLocalTurn();')).toBe(
      false
    );
    expect(persistedBeforeDispatch > explicitHandler).toBe(true);
    expect(explicitDispatch > persistedBeforeDispatch).toBe(true);
    expect(
      openClawSource
        .slice(explicitHandler, remountRead)
        .includes('deliverStarOfficeRequest(delivery, storageKey, true)')
    ).toBe(false);
    expect(remountRead > explicitDispatch).toBe(true);
    expect(terminalFence > remountRead).toBe(true);
    expect(quarantine > terminalFence).toBe(true);
    expect(runningBranch > quarantine).toBe(true);
    expect(runningReplay > runningBranch).toBe(true);
    expect(pendingAuthority > runningReplay).toBe(true);
    expect(pendingInitialOnly > pendingAuthority).toBe(true);
    expect(
      openClawSource
        .slice(remountRead, pendingInitialOnly)
        .includes('deliverStarOfficeRequest(pending, storageKey);')
    ).toBe(false);
  });

  test('keeps persisted initial deliveries closed until a fresh accepted response', () => {
    const initialCallers = [
      {
        source: readSource('./acp/useAcpInitialMessage.ts'),
        start: 'const sendInitialMessage = async () => {',
        post: 'sendMessage.invoke({',
        open: 'setAiProcessing(true);',
      },
      {
        source: remoteSource,
        start: 'const processInitialMessage = async () => {',
        post: 'sendMessage.invoke({',
        open: 'beginLocalTurn();',
      },
      {
        source: nanobotSource,
        start: 'const processInitialMessage = async () => {',
        post: 'sendMessage.invoke({',
        open: 'beginLocalTurn();',
      },
      {
        source: openClawSource,
        start: '// Handle initial message from guid page.',
        post: 'sendMessage.invoke({',
        open: 'beginLocalTurn();',
      },
    ];

    for (const { source, start: startMarker, post: postMarker, open } of initialCallers) {
      const start = source.indexOf(startMarker);
      const post = source.indexOf(postMarker, start);
      const classification = source.indexOf(
        'const disposition = classifyPublicMessageDelivery(',
        post
      );
      const fresh = source.indexOf("if (disposition === 'fresh') {", classification);
      const freshOpen = source.indexOf(open, fresh);
      const preResponse = source.slice(start, post);

      expect(start >= 0).toBe(true);
      expect(post > start).toBe(true);
      expect(preResponse.includes('beginLocalTurn();')).toBe(false);
      expect(preResponse.includes('setAiProcessing(true);')).toBe(false);
      expect(preResponse.includes('setWaitingResponse(true);')).toBe(false);
      expect(fresh > post).toBe(true);
      expect(freshOpen > fresh).toBe(true);
    }

    const nomiInitial = nomiSource.indexOf('const processInitialMessage = async () => {');
    const nomiDeferredDispatch = nomiSource.indexOf(
      '{ id: idempotency_key, input, files, initialOnly: true }',
      nomiInitial
    );
    expect(nomiInitial >= 0).toBe(true);
    expect(nomiDeferredDispatch > nomiInitial).toBe(true);
  });

  test('keeps edit replays behind authoritative reconciliation', () => {
    const post = nomiSource.indexOf('editResubmit.invoke({');
    const classification = nomiSource.indexOf(
      'const disposition = classifyPublicMessageDelivery(res);',
      post
    );
    const fresh = nomiSource.indexOf("if (disposition === 'fresh') {", classification);
    const replayBranch = nomiSource.indexOf('} else {', fresh);
    const closeOrReconcile = nomiSource.indexOf(
      'reconcilePublicDeliveryReplay(res.completed);',
      replayBranch
    );

    expect(post >= 0).toBe(true);
    expect(classification > post).toBe(true);
    expect(fresh > classification).toBe(true);
    expect(replayBranch > fresh).toBe(true);
    expect(closeOrReconcile > replayBranch).toBe(true);
  });

  test('does not confuse a completed steer receipt with parent turn completion', () => {
    const post = nomiSource.indexOf('steer.invoke({');
    const classification = nomiSource.indexOf(
      'const disposition = classifyPublicMessageDelivery(res);',
      post
    );
    const acceptedReplay = nomiSource.indexOf(
      "} else if (disposition === 'replayed_in_flight') {",
      classification
    );
    const authoritativeRead = nomiSource.indexOf(
      'reconcilePublicDeliveryReplay(false);',
      acceptedReplay
    );
    const completedReplay = nomiSource.indexOf('} else {', authoritativeRead);
    const completedReplayEnd = nomiSource.indexOf("emitter.emit('chat.history.refresh');", completedReplay);
    const completedReplaySource = nomiSource.slice(completedReplay, completedReplayEnd);

    expect(post >= 0).toBe(true);
    expect(classification > post).toBe(true);
    expect(acceptedReplay > classification).toBe(true);
    expect(authoritativeRead > acceptedReplay).toBe(true);
    expect(completedReplay > authoritativeRead).toBe(true);
    expect(completedReplayEnd > completedReplay).toBe(true);
    expect(completedReplaySource.includes('setActiveMsgId(null);')).toBe(true);
    expect(completedReplaySource.includes('reconcileAfterStreamTerminal();')).toBe(true);
    expect(completedReplaySource.includes('reconcilePublicDeliveryReplay(')).toBe(false);
    expect(completedReplaySource.includes('setWaitingResponse(false)')).toBe(false);
  });

  test('keeps companion completed replays out of task-running UI', () => {
    const companionSource = readSource('../../companion/index.tsx');
    const delivery = companionSource.indexOf('const deliverTurn = useCallback(');
    const post = companionSource.indexOf('sendMessage.invoke({', delivery);
    const preResponse = companionSource.slice(delivery, post);
    const classification = companionSource.indexOf(
      'const disposition = classifyPublicMessageDelivery(sendResult);',
      post
    );
    const replayBranch = companionSource.indexOf(
      "if (disposition !== 'fresh') {",
      classification
    );
    const replayReturn = companionSource.indexOf('return;', replayBranch);
    const freshOpen = companionSource.indexOf(
      'turnActiveRef.current = true;',
      replayReturn
    );
    const freshStreamUnfence = companionSource.lastIndexOf(
      'bubbleDismissedRef.current = false;',
      freshOpen
    );

    expect(delivery >= 0).toBe(true);
    expect(post > delivery).toBe(true);
    expect(preResponse.includes('setSending(true);')).toBe(false);
    expect(preResponse.includes('setBubbleLoading(true);')).toBe(false);
    expect(preResponse.includes('setBubbleRunning(true);')).toBe(false);
    expect(preResponse.includes('turnActiveRef.current = true;')).toBe(false);
    expect(preResponse.includes('bubbleDismissedRef.current = true;')).toBe(true);
    expect(replayBranch > classification).toBe(true);
    expect(replayReturn > replayBranch).toBe(true);
    expect(freshStreamUnfence > replayReturn).toBe(true);
    expect(freshOpen > replayReturn).toBe(true);
  });
});
