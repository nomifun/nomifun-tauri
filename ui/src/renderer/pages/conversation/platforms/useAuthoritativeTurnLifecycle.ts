import { ipcBridge } from '@/common';
import type { ConversationId, MessageId } from '@/common/types/ids';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { isConversationProcessing } from '@/renderer/pages/conversation/utils/conversationRuntime';
import { useCallback, useEffect, useRef } from 'react';
import { reconcileConversationTurnAfterStreamTerminal } from './reconcileConversationTurnAfterStreamTerminal';
import {
  classifyAuthoritativeTurnCompletion,
  classifyAuthoritativeTurnStart,
  resolveVerifiedAuthoritativeTurnStart,
} from './authoritativeTurnLifecyclePolicy';

type AuthoritativeTurnLifecycleOptions = {
  onTurnStarted?: () => void;
  onTurnCompleted: () => void;
};

/**
 * Owns the generation/correlation bookkeeping shared by the simple composer
 * surfaces. Stream `finish`/`error` events close message rendering only; the UI
 * busy state is lowered exclusively by the conversation-scoped turn.completed
 * event after the backend has released its turn handle.
 */
export const useAuthoritativeTurnLifecycle = (
  conversationId: ConversationId,
  { onTurnStarted, onTurnCompleted }: AuthoritativeTurnLifecycleOptions
) => {
  const onTurnStartedRef = useRef(onTurnStarted);
  const onTurnCompletedRef = useRef(onTurnCompleted);
  const rootTurnIdRef = useRef<MessageId | null>(null);
  const awaitingBackendTurnRef = useRef(false);
  const closedRef = useRef(false);
  const generationRef = useRef(0);
  const turnStartGenerationRef = useRef(0);
  const turnCompletionGenerationRef = useRef(0);
  const reconcileSequenceRef = useRef(0);
  const cancelledTurnIdsRef = useRef(new Set<MessageId>());
  const rejectUnannouncedStartRef = useRef(false);
  const verifyUnannouncedStartRuntimeRef = useRef(false);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      generationRef.current += 1;
      reconcileSequenceRef.current += 1;
    };
  }, []);

  useEffect(() => {
    onTurnStartedRef.current = onTurnStarted;
    onTurnCompletedRef.current = onTurnCompleted;
  }, [onTurnCompleted, onTurnStarted]);

  useEffect(() => {
    rootTurnIdRef.current = null;
    awaitingBackendTurnRef.current = false;
    closedRef.current = false;
    generationRef.current += 1;
    reconcileSequenceRef.current += 1;
    cancelledTurnIdsRef.current.clear();
    rejectUnannouncedStartRef.current = false;
    verifyUnannouncedStartRuntimeRef.current = false;
  }, [conversationId]);

  const beginLocalTurn = useCallback(() => {
    turnStartGenerationRef.current += 1;
    generationRef.current += 1;
    rootTurnIdRef.current = null;
    awaitingBackendTurnRef.current = true;
    closedRef.current = false;
    rejectUnannouncedStartRef.current = false;
    reconcileSequenceRef.current += 1;
  }, []);

  const cancelLocalTurn = useCallback(() => {
    generationRef.current += 1;
    rootTurnIdRef.current = null;
    awaitingBackendTurnRef.current = false;
    closedRef.current = true;
    rejectUnannouncedStartRef.current = false;
    reconcileSequenceRef.current += 1;
  }, []);

  const stopOptimistically = useCallback(() => {
    generationRef.current += 1;
    const rootTurnId = rootTurnIdRef.current;
    if (rootTurnId) {
      const cancelled = cancelledTurnIdsRef.current;
      cancelled.add(rootTurnId);
      if (cancelled.size > 32) {
        const oldest = cancelled.values().next().value;
        if (oldest) cancelled.delete(oldest);
      }
    }
    awaitingBackendTurnRef.current = false;
    closedRef.current = true;
    rejectUnannouncedStartRef.current = true;
    verifyUnannouncedStartRuntimeRef.current = rootTurnId === null;
    reconcileSequenceRef.current += 1;
  }, []);

  const acceptsStreamActivity = useCallback(
    () => !closedRef.current || awaitingBackendTurnRef.current,
    []
  );

  const settle = useCallback((expectedGeneration: number) => {
    if (!mountedRef.current || generationRef.current !== expectedGeneration) return;
    generationRef.current += 1;
    turnCompletionGenerationRef.current += 1;
    reconcileSequenceRef.current += 1;
    rootTurnIdRef.current = null;
    awaitingBackendTurnRef.current = false;
    closedRef.current = true;
    rejectUnannouncedStartRef.current = false;
    onTurnCompletedRef.current();
  }, []);

  const confirmStopped = useCallback(() => {
    generationRef.current += 1;
    reconcileSequenceRef.current += 1;
    rootTurnIdRef.current = null;
    awaitingBackendTurnRef.current = false;
    closedRef.current = true;
    rejectUnannouncedStartRef.current = false;
    onTurnCompletedRef.current();
  }, []);

  const reconcileGeneration = useCallback(
    (generation: number, delaysMs?: readonly number[], isExpected: () => boolean = () => true) => {
      const sequence = reconcileSequenceRef.current + 1;
      reconcileSequenceRef.current = sequence;
      void reconcileConversationTurnAfterStreamTerminal(
        conversationId,
        () =>
          mountedRef.current &&
          generationRef.current === generation &&
          reconcileSequenceRef.current === sequence &&
          isExpected(),
        () => settle(generation),
        delaysMs
      );
    },
    [conversationId, settle]
  );

  const restoreAfterStopFailure = useCallback(() => {
    generationRef.current += 1;
    const rootTurnId = rootTurnIdRef.current;
    if (rootTurnId) cancelledTurnIdsRef.current.delete(rootTurnId);
    awaitingBackendTurnRef.current = false;
    closedRef.current = false;
    rejectUnannouncedStartRef.current = false;
    verifyUnannouncedStartRuntimeRef.current = false;
    // A null-turn completion may already be awaiting its own GET when the stop
    // request fails. Restoring advances the lifecycle generation, so start a
    // fresh authoritative reconciliation instead of losing that idle signal.
    reconcileGeneration(generationRef.current);
  }, [reconcileGeneration]);

  const markLocalTurnAccepted = useCallback(
    () => {
      if (!awaitingBackendTurnRef.current || closedRef.current) return;
      // Do not invalidate an in-flight runtime verification inherited from an
      // unknown-root stop. The verified start itself advances the generation.
      if (!verifyUnannouncedStartRuntimeRef.current) generationRef.current += 1;
      reconcileSequenceRef.current += 1;
      // The POST returns the user-message id, while wire turn_id identifies the
      // first assistant turn message. Correlation is established exclusively by
      // turn.started; acceptance only unlocks authoritative runtime recovery.
      rootTurnIdRef.current = null;
      awaitingBackendTurnRef.current = false;
      const generation = generationRef.current;
      // Covers a fast turn whose started/completed or terminal stream events
      // raced the HTTP result. Use the same bounded long-tail window as terminal
      // reconciliation so slow DB/receipt finalization cannot strand busy state.
      reconcileGeneration(generation);
    },
    [reconcileGeneration]
  );

  useEffect(() => {
    let disposed = false;
    const unsubscribe = ipcBridge.conversation.turnStarted.on((event) => {
      if (event.conversation_id !== conversationId) return;
      const startAction = classifyAuthoritativeTurnStart({
        turnId: event.turn_id,
        cancelledTurnIds: cancelledTurnIdsRef.current,
        rejectUnannouncedStart: rejectUnannouncedStartRef.current,
        awaitingBackendTurn: awaitingBackendTurnRef.current,
        verifyUnannouncedStartRuntime: verifyUnannouncedStartRuntimeRef.current,
      });
      if (startAction === 'ignore') return;

      const acceptStart = () => {
        turnStartGenerationRef.current += 1;
        generationRef.current += 1;
        reconcileSequenceRef.current += 1;
        rootTurnIdRef.current = event.turn_id ?? null;
        awaitingBackendTurnRef.current = false;
        closedRef.current = false;
        rejectUnannouncedStartRef.current = false;
        verifyUnannouncedStartRuntimeRef.current = false;
        onTurnStartedRef.current?.();
      };

      if (startAction === 'accept') {
        acceptStart();
        return;
      }

      const generation = generationRef.current;
      void getConversationOrNull(conversationId)
        .then((conversation) => {
          if (
            disposed ||
            generationRef.current !== generation ||
            !verifyUnannouncedStartRuntimeRef.current ||
            resolveVerifiedAuthoritativeTurnStart({
              runtimeIsProcessing: isConversationProcessing(conversation),
              eventProcessingStartedAt: event.runtime.processing_started_at,
              runtimeProcessingStartedAt: conversation?.runtime?.processing_started_at,
            }) !== 'accept'
          ) {
            return;
          }
          acceptStart();
        })
        .catch((error) => {
          if (disposed) return;
          console.warn('[conversation-turn-lifecycle] Failed to verify unannounced turn start:', error);
        });
    });
    return () => {
      disposed = true;
      unsubscribe();
    };
  }, [conversationId]);

  useEffect(() => {
    const unsubscribe = ipcBridge.conversation.turnCompleted.on((event) => {
      if (event.conversation_id !== conversationId || event.runtime.is_processing) return;

      const generation = generationRef.current;
      const rootTurnId = rootTurnIdRef.current;
      const awaitingBackendTurn = awaitingBackendTurnRef.current;
      const action = classifyAuthoritativeTurnCompletion({
        rootTurnId,
        completedTurnId: event.turn_id,
        awaitingBackendTurn,
      });
      if (action === 'settle') {
        settle(generation);
        return;
      }
      if (action === 'ignore') return;

      reconcileGeneration(generation, undefined, () =>
        rootTurnIdRef.current === rootTurnId &&
        awaitingBackendTurnRef.current === awaitingBackendTurn
      );
    });

    return () => {
      unsubscribe();
    };
  }, [conversationId, reconcileGeneration, settle]);

  const reconcileAfterStreamTerminal = useCallback(() => {
    const generation = generationRef.current;
    if (awaitingBackendTurnRef.current) return;
    reconcileGeneration(generation);
  }, [reconcileGeneration]);

  const getTurnStartGeneration = useCallback(() => turnStartGenerationRef.current, []);
  const getTurnCompletionGeneration = useCallback(() => turnCompletionGenerationRef.current, []);
  const getTurnLifecycleGeneration = useCallback(() => generationRef.current, []);

  return {
    beginLocalTurn,
    markLocalTurnAccepted,
    cancelLocalTurn,
    stopOptimistically,
    confirmStopped,
    restoreAfterStopFailure,
    acceptsStreamActivity,
    reconcileAfterStreamTerminal,
    getTurnStartGeneration,
    getTurnCompletionGeneration,
    getTurnLifecycleGeneration,
  };
};
