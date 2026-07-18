import { ipcBridge } from '@/common';
import type { ConversationId } from '@/common/types/ids';
import { conversationTarget } from '@/common/types/ids';
import { sessionStorageKey } from '@/common/utils/browserStorageKey';
import { uuid } from '@/common/utils';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { isConversationProcessing } from '@/renderer/pages/conversation/utils/conversationRuntime';
import { useAddEventListener } from '@/renderer/utils/emitter';
import { Message } from '@arco-design/web-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import useSWR from 'swr';
import {
  getCommandQueueReconcileDelayMs,
  IDLE_EXECUTION_GATE,
  isCommandQueueExecutionCurrent,
  reduceCommandQueueExecutionGate,
  type CommandQueueExecutionGate,
} from './commandQueueExecutionGate';

export {
  reduceCommandQueueExecutionGate,
  type CommandQueueExecutionGate,
  type CommandQueueExecutionGateEvent,
} from './commandQueueExecutionGate';

export type ConversationCommandQueueItem = {
  id: string;
  input: string;
  files: string[];
  created_at: number;
};

export type ConversationCommandQueueState = {
  items: ConversationCommandQueueItem[];
  isPaused: boolean;
};

export const MAX_QUEUED_COMMANDS = 20;
export const MAX_QUEUED_COMMAND_INPUT_LENGTH = 20_000;
export const MAX_QUEUED_COMMAND_FILES = 50;
export const MAX_QUEUED_COMMAND_STATE_BYTES = 256 * 1024;
export const COMMAND_QUEUE_RUNTIME_QUERY_TIMEOUT_MS = 3_000;

const getConversationForCommandQueue = async (conversationId: ConversationId) => {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      getConversationOrNull(conversationId),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error('Command queue runtime reconciliation timed out')),
          COMMAND_QUEUE_RUNTIME_QUERY_TIMEOUT_MS
        );
      }),
    ]);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
};

export type QueueValidationFailureReason =
  | 'emptyInput'
  | 'inputTooLong'
  | 'tooManyFiles'
  | 'queueFull'
  | 'queueTooLarge';

type QueueValidationSuccess = {
  ok: true;
  nextStateBytes: number;
};

type QueueValidationFailure = {
  ok: false;
  reason: QueueValidationFailureReason;
};

const COMMAND_QUEUE_LOG_PREFIX = '[conversation-command-queue]';

const summarizeQueuedCommand = (item: ConversationCommandQueueItem): Record<string, unknown> => ({
  id: item.id,
  created_at: item.created_at,
  inputLength: item.input.length,
  fileCount: item.files.length,
  preview: item.input.replace(/\s+/g, ' ').trim().slice(0, 120),
});

const logCommandQueue = (conversation_id: ConversationId, event: string, payload: Record<string, unknown> = {}): void => {
  console.info(COMMAND_QUEUE_LOG_PREFIX, {
    conversation_id,
    event,
    ...payload,
  });
};

const createDefaultQueueState = (): ConversationCommandQueueState => ({
  items: [],
  isPaused: false,
});

const queueStore = new Map<ConversationId, ConversationCommandQueueState>();

const getStorageKey = (conversation_id: ConversationId): string =>
  sessionStorageKey('command-queue', conversationTarget(conversation_id));
const measureQueueStateBytes = (state: ConversationCommandQueueState): number =>
  new TextEncoder().encode(JSON.stringify(state)).length;

const uniqueFiles = (files: string[]): string[] => Array.from(new Set(files.filter(Boolean)));
const isInputEmpty = (input: string): boolean => input.trim().length === 0;

const normalizeQueueItem = (item: unknown): ConversationCommandQueueItem | null => {
  if (!item || typeof item !== 'object') {
    return null;
  }

  const candidate = item as Record<string, unknown>;
  if (
    typeof candidate.id !== 'string' ||
    typeof candidate.input !== 'string' ||
    !Array.isArray(candidate.files) ||
    !candidate.files.every((file) => typeof file === 'string') ||
    typeof candidate.created_at !== 'number' ||
    !Number.isFinite(candidate.created_at)
  ) {
    return null;
  }

  const normalizedItem: ConversationCommandQueueItem = {
    id: candidate.id,
    input: candidate.input,
    files: uniqueFiles(candidate.files),
    created_at: candidate.created_at,
  };

  if (
    isInputEmpty(normalizedItem.input) ||
    normalizedItem.input.length > MAX_QUEUED_COMMAND_INPUT_LENGTH ||
    normalizedItem.files.length > MAX_QUEUED_COMMAND_FILES
  ) {
    return null;
  }

  return normalizedItem;
};

export const normalizeQueueState = (state: unknown): ConversationCommandQueueState => {
  if (!state || typeof state !== 'object') {
    return createDefaultQueueState();
  }

  const candidate = state as Partial<ConversationCommandQueueState>;
  const normalizedItems = Array.isArray(candidate.items)
    ? candidate.items.map(normalizeQueueItem).filter((item): item is ConversationCommandQueueItem => item !== null)
    : [];
  const items: ConversationCommandQueueItem[] = [];

  for (const item of normalizedItems.slice(0, MAX_QUEUED_COMMANDS)) {
    const nextItems = [...items, item];
    const nextState = {
      items: nextItems,
      isPaused: Boolean(candidate.isPaused),
    };

    if (measureQueueStateBytes(nextState) > MAX_QUEUED_COMMAND_STATE_BYTES) {
      break;
    }

    items.push(item);
  }

  return {
    items,
    isPaused: items.length > 0 ? Boolean(candidate.isPaused) : false,
  };
};

export const createQueuedCommandItem = ({
  input,
  files,
}: Pick<ConversationCommandQueueItem, 'input' | 'files'>): ConversationCommandQueueItem => ({
  id: uuid(),
  input,
  files: uniqueFiles(files),
  created_at: Date.now(),
});

const getQueueValidationFailureReason = (state: ConversationCommandQueueState): QueueValidationFailureReason | null => {
  if (state.items.length > MAX_QUEUED_COMMANDS) {
    return 'queueFull';
  }

  if (state.items.some((item) => isInputEmpty(item.input))) {
    return 'emptyInput';
  }

  if (state.items.some((item) => item.input.length > MAX_QUEUED_COMMAND_INPUT_LENGTH)) {
    return 'inputTooLong';
  }

  if (state.items.some((item) => item.files.length > MAX_QUEUED_COMMAND_FILES)) {
    return 'tooManyFiles';
  }

  if (measureQueueStateBytes(state) > MAX_QUEUED_COMMAND_STATE_BYTES) {
    return 'queueTooLarge';
  }

  return null;
};

export const validateQueuedCommandItem = (
  item: ConversationCommandQueueItem,
  state: ConversationCommandQueueState
): QueueValidationSuccess | QueueValidationFailure => {
  const nextState = {
    ...state,
    items: [...state.items, item],
  };
  const failureReason = getQueueValidationFailureReason(nextState);
  if (failureReason) {
    return { ok: false, reason: failureReason };
  }
  const nextStateBytes = measureQueueStateBytes(nextState);
  return { ok: true, nextStateBytes };
};

const isQueueValidationFailure = (
  validation: QueueValidationSuccess | QueueValidationFailure
): validation is QueueValidationFailure => !validation.ok;

const readPersistedQueueState = (conversation_id: ConversationId): ConversationCommandQueueState => {
  if (queueStore.has(conversation_id)) {
    return queueStore.get(conversation_id) ?? createDefaultQueueState();
  }

  if (typeof window === 'undefined') {
    return createDefaultQueueState();
  }

  try {
    const stored = window.sessionStorage.getItem(getStorageKey(conversation_id));
    if (!stored) {
      return createDefaultQueueState();
    }

    const parsed = JSON.parse(stored) as unknown;
    const normalized = normalizeQueueState(parsed);
    queueStore.set(conversation_id, normalized);
    logCommandQueue(conversation_id, 'restored', {
      itemCount: normalized.items.length,
      isPaused: normalized.isPaused,
    });
    return normalized;
  } catch (error) {
    console.warn('[conversation-command-queue] Failed to read persisted queue state:', error);
    return createDefaultQueueState();
  }
};

const removePersistedQueueState = (conversation_id: ConversationId): void => {
  queueStore.delete(conversation_id);
  if (typeof window !== 'undefined') {
    try {
      window.sessionStorage.removeItem(getStorageKey(conversation_id));
    } catch (error) {
      console.warn('[conversation-command-queue] Failed to remove persisted queue state:', error);
    }
  }
};

const persistQueueState = (conversation_id: ConversationId, state: ConversationCommandQueueState): void => {
  const normalized = normalizeQueueState(state);

  if (normalized.items.length === 0 && !normalized.isPaused) {
    removePersistedQueueState(conversation_id);
    return;
  }

  queueStore.set(conversation_id, normalized);
  if (typeof window !== 'undefined') {
    try {
      window.sessionStorage.setItem(getStorageKey(conversation_id), JSON.stringify(normalized));
    } catch (error) {
      console.warn('[conversation-command-queue] Failed to persist queue state:', error);
    }
  }
};

export const removeQueuedCommand = (
  items: ConversationCommandQueueItem[],
  commandId: string
): ConversationCommandQueueItem[] => items.filter((item) => item.id !== commandId);

export const reorderQueuedCommand = (
  items: ConversationCommandQueueItem[],
  activeCommandId: string,
  overCommandId: string
): ConversationCommandQueueItem[] => {
  const fromIndex = items.findIndex((item) => item.id === activeCommandId);
  const targetIndex = items.findIndex((item) => item.id === overCommandId);

  if (fromIndex === -1 || targetIndex === -1 || fromIndex === targetIndex) {
    return items;
  }

  const nextItems = [...items];
  const [movedItem] = nextItems.splice(fromIndex, 1);
  nextItems.splice(targetIndex, 0, movedItem);
  return nextItems;
};

export const restoreQueuedCommand = (
  items: ConversationCommandQueueItem[],
  failedItem: ConversationCommandQueueItem
): ConversationCommandQueueItem[] => [failedItem, ...removeQueuedCommand(items, failedItem.id)];

export const updateQueuedCommand = (
  items: ConversationCommandQueueItem[],
  commandId: string,
  updates: Partial<Pick<ConversationCommandQueueItem, 'input' | 'files'>>
): ConversationCommandQueueItem[] =>
  items.map((item) =>
    item.id === commandId
      ? {
          ...item,
          ...updates,
          files: updates.files ? uniqueFiles(updates.files) : item.files,
        }
      : item
  );

export const shouldEnqueueConversationCommand = ({
  enabled = true,
  isBusy,
  hasPendingCommands,
}: {
  enabled?: boolean;
  isBusy: boolean;
  hasPendingCommands: boolean;
}): boolean => enabled && (isBusy || hasPendingCommands);

type UseConversationCommandQueueOptions = {
  conversation_id: ConversationId;
  enabled?: boolean;
  isBusy: boolean;
  isHydrated?: boolean;
  onExecute: (
    item: ConversationCommandQueueItem,
    execution?: ConversationCommandQueueExecution
  ) => Promise<void>;
};

export type ConversationCommandQueueExecution = {
  isCurrent: () => boolean;
};

type EnqueueCommandInput = Pick<ConversationCommandQueueItem, 'input' | 'files'>;
type UpdateCommandInput = Pick<ConversationCommandQueueItem, 'input'>;

const getQueueValidationMessage = (
  t: (key: string, options?: Record<string, unknown>) => string,
  reason: QueueValidationFailureReason
): string => {
  const warningKeyMap = {
    emptyInput: 'conversation.commandQueue.emptyInput',
    queueFull: 'conversation.commandQueue.queueFull',
    inputTooLong: 'conversation.commandQueue.inputTooLong',
    tooManyFiles: 'conversation.commandQueue.tooManyFiles',
    queueTooLarge: 'conversation.commandQueue.queueTooLarge',
  } as const;
  const defaultValueMap = {
    emptyInput: 'Queued commands cannot be empty.',
    queueFull: 'Queue is full. Remove a command before adding more.',
    inputTooLong: 'This queued command is too long. Shorten it before sending.',
    tooManyFiles: 'Too many files are attached to this queued command.',
    queueTooLarge: 'Queue data is too large to persist safely. Remove some queued commands first.',
  } as const;

  return t(warningKeyMap[reason], {
    count: MAX_QUEUED_COMMANDS,
    files: MAX_QUEUED_COMMAND_FILES,
    defaultValue: defaultValueMap[reason],
  });
};

export const useConversationCommandQueue = ({
  conversation_id,
  enabled = true,
  isBusy,
  isHydrated = true,
  onExecute,
}: UseConversationCommandQueueOptions) => {
  const { t } = useTranslation();
  // Internal persistence/logging is keyed by the canonical conversation ID
  // (SWR key, sessionStorage key, and queueStore Map key).
  const conversationKey = conversation_id;
  const { data = createDefaultQueueState(), mutate } = useSWR(
    [`/conversation-command-queue/${conversationKey}`, conversationKey, enabled],
    ([, id, is_enabled]) => (is_enabled ? readPersistedQueueState(id) : createDefaultQueueState())
  );

  const stateRef = useRef(data);
  const pausedRef = useRef(data.isPaused);
  const executionGateRef = useRef<CommandQueueExecutionGate>(IDLE_EXECUTION_GATE);
  const executionGenerationRef = useRef(0);
  const executionConversationKeyRef = useRef(conversationKey);
  const mountedRef = useRef(true);
  const interactionLockedRef = useRef(false);
  const [isInteractionLocked, setIsInteractionLocked] = useState(false);
  const [executionGateVersion, setExecutionGateVersion] = useState(0);

  // Update during render so a promise owned by the previous conversation is
  // stale before passive effect cleanup has a chance to run.
  executionConversationKeyRef.current = conversationKey;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      executionGenerationRef.current += 1;
    };
  }, []);

  useEffect(() => {
    executionGenerationRef.current += 1;
    executionGateRef.current = IDLE_EXECUTION_GATE;
    pausedRef.current = false;
    interactionLockedRef.current = false;
    setIsInteractionLocked(false);
    setExecutionGateVersion((version) => version + 1);

    return () => {
      executionGenerationRef.current += 1;
    };
  }, [conversationKey]);

  const reconcileActiveExecution = useCallback(async (): Promise<boolean> => {
    const gate = executionGateRef.current;
    if (gate.phase === 'idle') return true;

    try {
      const conversation = await getConversationForCommandQueue(conversationKey);
      if (executionGateRef.current !== gate) return false;
      const isProcessing = isConversationProcessing(conversation);
      // If turn.started was lost, the accepted-send gate remains in
      // waiting_start. A later authoritative idle read must reconcile that
      // phase as a start acknowledgement; treating it as completion would
      // intentionally leave the gate closed forever.
      const purpose = gate.phase === 'waiting_start' ? 'start' : 'completion';
      const nextGate = reduceCommandQueueExecutionGate(gate, {
        type: 'runtimeReconciled',
        purpose,
        runtimeIsProcessing: isProcessing,
      });
      if (nextGate === gate) return false;
      executionGateRef.current = nextGate;
      logCommandQueue(conversationKey, 'execution-reconciled', {
        purpose,
        runtimeIsProcessing: isProcessing,
        nextPhase: nextGate.phase,
        pendingItemCount: stateRef.current.items.length,
      });
      setExecutionGateVersion((version) => version + 1);
      return true;
    } catch (error) {
      console.warn('[conversation-command-queue] Failed to reconcile active execution:', error);
      return false;
    }
  }, [conversationKey]);

  useEffect(() => {
    stateRef.current = data;
  }, [data]);

  useEffect(() => {
    let disposed = false;

    const releaseExecutionGate = (expectedGate: CommandQueueExecutionGate, source: 'correlated' | 'runtime') => {
      if (disposed || executionGateRef.current !== expectedGate) return;
      executionGateRef.current = IDLE_EXECUTION_GATE;
      logCommandQueue(conversationKey, 'turn-completed', {
        source,
        pendingItemCount: stateRef.current.items.length,
      });
      setExecutionGateVersion((version) => version + 1);
    };

    const unsubscribeStarted = ipcBridge.conversation.turnStarted.on((event) => {
      if (event.conversation_id !== conversationKey) return;

      const previousGate = executionGateRef.current;
      const nextGate = reduceCommandQueueExecutionGate(previousGate, {
        type: 'turnStarted',
        turnId: event.turn_id,
      });
      if (nextGate === previousGate) return;

      executionGateRef.current = nextGate;
      logCommandQueue(conversationKey, 'turn-started', {
        turnId: event.turn_id,
        pendingItemCount: stateRef.current.items.length,
      });
      setExecutionGateVersion((version) => version + 1);
    });

    const unsubscribeCompleted = ipcBridge.conversation.turnCompleted.on((event) => {
      if (event.conversation_id !== conversationKey || event.runtime.is_processing) return;

      const gate = executionGateRef.current;
      if (gate.phase === 'idle') return;

      // A completion racing a newly submitted command still waiting for its
      // start acknowledgement may belong to the previous generation. Only the
      // send promise's accepted-result reconciliation may advance this phase.
      if (gate.phase === 'waiting_start') return;

      // New servers provide an exact generation id. Never let a delayed
      // completion from an older turn release the gate for a newer turn.
      const completedGate = reduceCommandQueueExecutionGate(gate, {
        type: 'turnCompleted',
        turnId: event.turn_id,
        runtimeIsProcessing: event.runtime.is_processing,
      });
      if (completedGate.phase === 'idle') {
        releaseExecutionGate(gate, 'correlated');
        return;
      }

      // A mismatched correlated completion belongs to an older generation and
      // must never be upgraded into an uncorrelated runtime release.
      if (gate.phase === 'waiting_completion' && gate.turnId && event.turn_id) return;

      // Backward compatibility for servers that predate turn_id on completion:
      // re-read the authoritative runtime after the event. The identity check in
      // releaseExecutionGate also rejects a start/reset that races this request.
      void reconcileActiveExecution();
    });

    return () => {
      disposed = true;
      unsubscribeStarted();
      unsubscribeCompleted();
    };
  }, [conversationKey, reconcileActiveExecution]);

  useEffect(() => {
    pausedRef.current = data.isPaused;
  }, [data.isPaused]);

  // A manually-started turn (or a running conversation restored on mount) also
  // owns the queue gate. Queue items added during that turn must wait for the
  // authoritative completion event, not the earlier visual spinner down-edge.
  useEffect(() => {
    if (!isBusy || executionGateRef.current.phase !== 'idle') return;
    executionGateRef.current = { phase: 'waiting_completion' };
    setExecutionGateVersion((version) => version + 1);
  }, [isBusy]);

  // A missing turn.completed event must not strand the queue forever. The
  // visual busy down-edge only schedules reconciliation; the gate is released
  // after GET confirms the backend runtime is no longer processing. Failed or
  // timed-out reads keep retrying with a capped backoff for as long as this
  // idle UI still owns a non-idle gate, so service recovery always converges.
  useEffect(() => {
    if (isBusy || executionGateRef.current.phase === 'idle') return;
    let cancelled = false;

    void (async () => {
      let attempt = 0;
      while (!cancelled) {
        const delayMs = getCommandQueueReconcileDelayMs(attempt);
        await new Promise<void>((resolve) => setTimeout(resolve, delayMs));
        if (cancelled) return;
        if (await reconcileActiveExecution()) return;
        attempt += 1;
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [executionGateVersion, isBusy, reconcileActiveExecution]);

  useEffect(() => {
    interactionLockedRef.current = isInteractionLocked;
  }, [isInteractionLocked]);

  useEffect(() => {
    if (enabled) {
      return;
    }

    executionGateRef.current = IDLE_EXECUTION_GATE;
    executionGenerationRef.current += 1;
    pausedRef.current = false;
    interactionLockedRef.current = false;
    stateRef.current = createDefaultQueueState();
    setIsInteractionLocked(false);
    removePersistedQueueState(conversationKey);
    void mutate(createDefaultQueueState(), { revalidate: false });
  }, [conversation_id, enabled, mutate]);

  const updateState = useCallback(
    (
      updater: (state: ConversationCommandQueueState) => ConversationCommandQueueState
    ): Promise<ConversationCommandQueueState | undefined> => {
      if (!enabled) {
        const nextState = createDefaultQueueState();
        stateRef.current = nextState;
        pausedRef.current = false;
        removePersistedQueueState(conversationKey);
        return Promise.resolve(nextState);
      }

      return mutate(
        (current) => {
          const nextState = normalizeQueueState(updater(current ?? createDefaultQueueState()));
          stateRef.current = nextState;
          pausedRef.current = nextState.isPaused;
          persistQueueState(conversationKey, nextState);
          return nextState;
        },
        { revalidate: false }
      );
    },
    [conversation_id, enabled, mutate]
  );

  const clear = useCallback(() => {
    pausedRef.current = false;
    logCommandQueue(conversationKey, 'cleared');
    void updateState(() => createDefaultQueueState());
  }, [conversation_id, updateState]);

  useAddEventListener(
    'conversation.deleted',
    (deletedConversationId) => {
      if (deletedConversationId !== conversationKey) {
        return;
      }
      executionGenerationRef.current += 1;
      executionGateRef.current = IDLE_EXECUTION_GATE;
      clear();
      removePersistedQueueState(conversationKey);
    },
    [clear, conversation_id]
  );

  const enqueue = useCallback(
    ({ input, files }: EnqueueCommandInput) => {
      if (!enabled) {
        return null;
      }

      const currentState = normalizeQueueState(stateRef.current);
      const item = createQueuedCommandItem({ input, files });
      const validation = validateQueuedCommandItem(item, currentState);

      if (isQueueValidationFailure(validation)) {
        const reason: QueueValidationFailureReason = validation.reason;
        logCommandQueue(conversationKey, 'enqueue-rejected', {
          reason,
          item: summarizeQueuedCommand(item),
          currentItemCount: currentState.items.length,
        });
        Message.warning(getQueueValidationMessage(t, reason));
        return null;
      }

      const nextState: ConversationCommandQueueState = {
        ...currentState,
        items: [...currentState.items, item],
      };
      stateRef.current = nextState;
      logCommandQueue(conversationKey, 'enqueued', {
        item: summarizeQueuedCommand(item),
        currentItemCount: currentState.items.length,
      });
      void updateState(() => nextState);
      return item;
    },
    [conversation_id, enabled, t, updateState]
  );

  const update = useCallback(
    (commandId: string, { input }: UpdateCommandInput) => {
      if (!enabled) {
        return false;
      }

      const currentState = normalizeQueueState(stateRef.current);
      const currentItem = currentState.items.find((item) => item.id === commandId);
      if (!currentItem) {
        return false;
      }

      const nextItems = updateQueuedCommand(currentState.items, commandId, { input });
      const nextState: ConversationCommandQueueState = {
        isPaused: false,
        items: nextItems,
      };
      const failureReason = getQueueValidationFailureReason(nextState);

      if (failureReason) {
        logCommandQueue(conversationKey, 'update-rejected', {
          reason: failureReason,
          commandId,
          inputLength: input.length,
        });
        Message.warning(getQueueValidationMessage(t, failureReason));
        return false;
      }

      stateRef.current = nextState;
      logCommandQueue(conversationKey, 'updated', {
        commandId,
        inputLength: input.length,
      });
      void updateState(() => nextState);
      return true;
    },
    [conversation_id, enabled, t, updateState]
  );

  const remove = useCallback(
    (commandId: string) => {
      if (!enabled) {
        return;
      }

      logCommandQueue(conversationKey, 'removed', {
        commandId,
      });
      void updateState((state) => {
        const nextItems = removeQueuedCommand(state.items, commandId);
        return {
          items: nextItems,
          isPaused: false,
        };
      });
    },
    [conversation_id, enabled, updateState]
  );

  const reorder = useCallback(
    (activeCommandId: string, overCommandId: string) => {
      if (!enabled) {
        return;
      }

      logCommandQueue(conversationKey, 'reordered', {
        activeCommandId,
        overCommandId,
      });
      void updateState((state) => ({
        isPaused: false,
        items: reorderQueuedCommand(state.items, activeCommandId, overCommandId),
      }));
    },
    [conversation_id, enabled, updateState]
  );

  const pause = useCallback(() => {
    if (!enabled) {
      return;
    }

    pausedRef.current = true;
    logCommandQueue(conversationKey, 'paused', {
      itemCount: data.items.length,
    });
    void updateState((state) => {
      if (state.items.length === 0) {
        pausedRef.current = false;
        return createDefaultQueueState();
      }
      return {
        ...state,
        isPaused: true,
      };
    });
  }, [conversation_id, data.items.length, enabled, updateState]);

  const resume = useCallback(() => {
    if (!enabled) {
      return;
    }

    pausedRef.current = false;
    logCommandQueue(conversationKey, 'resumed', {
      itemCount: data.items.length,
    });
    void updateState((state) => ({
      ...state,
      isPaused: state.items.length > 0 ? false : state.isPaused,
    }));
  }, [conversation_id, data.items.length, enabled, updateState]);

  const lockInteraction = useCallback(() => {
    if (!enabled) {
      return;
    }

    interactionLockedRef.current = true;
    logCommandQueue(conversationKey, 'interaction-locked', {
      itemCount: stateRef.current.items.length,
    });
    setIsInteractionLocked(true);
  }, [conversation_id, enabled]);

  const unlockInteraction = useCallback(() => {
    if (!enabled) {
      return;
    }

    interactionLockedRef.current = false;
    logCommandQueue(conversationKey, 'interaction-unlocked', {
      itemCount: stateRef.current.items.length,
    });
    setIsInteractionLocked(false);
  }, [conversation_id, enabled]);

  const resetActiveExecution = useCallback(
    (reason: 'stop' | 'external-reset') => {
      executionGenerationRef.current += 1;
      const hadPendingTurn = executionGateRef.current.phase !== 'idle';

      // An optimistic stop may lower the visual busy flag before the backend has
      // released its turn handle. Keep the queue closed until turn.completed;
      // otherwise the next queued POST can still race the active turn and 409.
      if (reason === 'stop') {
        executionGateRef.current = reduceCommandQueueExecutionGate(executionGateRef.current, { type: 'stop' });
        logCommandQueue(conversationKey, 'execution-stop-pending', {
          pendingItemCount: stateRef.current.items.length,
        });
        setExecutionGateVersion((version) => version + 1);
        return;
      }

      executionGateRef.current = IDLE_EXECUTION_GATE;

      if (!hadPendingTurn) {
        return;
      }

      logCommandQueue(conversationKey, 'execution-reset', {
        reason,
        pendingItemCount: stateRef.current.items.length,
      });
      setExecutionGateVersion((version) => version + 1);
    },
    [conversation_id]
  );

  useEffect(() => {
    if (
      !enabled ||
      !isHydrated ||
      pausedRef.current ||
      isBusy ||
      executionGateRef.current.phase !== 'idle' ||
      interactionLockedRef.current ||
      data.items.length === 0
    ) {
      return;
    }

    const [nextCommand, ...remainingCommands] = data.items;
    const executionGeneration = executionGenerationRef.current + 1;
    executionGenerationRef.current = executionGeneration;
    const isExecutionCurrent = (): boolean =>
      isCommandQueueExecutionCurrent({
        mounted: mountedRef.current,
        currentConversationId: executionConversationKeyRef.current,
        expectedConversationId: conversationKey,
        currentGeneration: executionGenerationRef.current,
        expectedGeneration: executionGeneration,
      });
    executionGateRef.current = reduceCommandQueueExecutionGate(executionGateRef.current, { type: 'begin' });
    logCommandQueue(conversationKey, 'dequeued', {
      item: summarizeQueuedCommand(nextCommand),
      remainingItemCount: remainingCommands.length,
    });
    void updateState(() => ({
      items: remainingCommands,
      isPaused: false,
    }));

    void onExecute(nextCommand, { isCurrent: isExecutionCurrent })
      .then(() => {
        if (!isExecutionCurrent()) return;
        // turn.started normally moves the gate first. If that WS event was
        // missed, reconcile the accepted send against authoritative runtime so
        // the queue cannot remain in waiting_start forever. The same helper is
        // retried on the authoritative visual busy down-edge if this read fails.
        void reconcileActiveExecution();
      })
      .catch((error) => {
        if (!isExecutionCurrent() || executionGateRef.current.phase !== 'waiting_start') {
          return;
        }
        console.error('[conversation-command-queue] Failed to execute queued command:', error);
        logCommandQueue(conversationKey, 'execute-failed', {
          item: summarizeQueuedCommand(nextCommand),
          error: error instanceof Error ? error.message : String(error),
        });
        executionGateRef.current = IDLE_EXECUTION_GATE;
        pausedRef.current = true;
        void updateState((state) => ({
          items: restoreQueuedCommand(state.items, nextCommand),
          isPaused: true,
        }));
        Message.warning(
          t('conversation.commandQueue.pausedAfterFailure', {
            defaultValue: 'The next queued command could not start. Edit, reorder, or remove it to continue.',
          })
        );
      });
  }, [
    conversation_id,
    data.items,
    enabled,
    executionGateVersion,
    isBusy,
    isHydrated,
    isInteractionLocked,
    onExecute,
    reconcileActiveExecution,
    t,
    updateState,
  ]);

  return {
    items: enabled ? data.items : [],
    isPaused: enabled ? data.isPaused : false,
    isInteractionLocked,
    hasPendingCommands: enabled ? data.items.length > 0 : false,
    enqueue,
    update,
    remove,
    clear,
    reorder,
    pause,
    resume,
    lockInteraction,
    unlockInteraction,
    resetActiveExecution,
    reconcileActiveExecution,
  };
};
