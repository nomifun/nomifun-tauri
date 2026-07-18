/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ConversationId, MessageId } from '@/common/types/ids';
import { ipcBridge } from '@/common';
import {
  ACP_AGENT_MESSAGE_EVENT,
  normalizeWireAgentMessageMetadata,
  transformMessage,
} from '@/common/chat/chatLib';
import type { AvailableCommand } from '@/common/chat/chatLib';
import { toDisplayText } from '@/common/chat/displayText';
import type { SlashCommandItem } from '@/common/chat/slash/types';
import type { IResponseMessage } from '@/common/adapter/ipcBridge';
import type { TokenUsageData } from '@/common/config/storage';
import { useAddOrUpdateMessage } from '@/renderer/pages/conversation/Messages/hooks';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { isConversationProcessing } from '@/renderer/pages/conversation/utils/conversationRuntime';
import { warmupConversation } from '@/renderer/pages/conversation/utils/warmupConversation';
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react';
import type { ThoughtData } from '../thoughtTypes';
import { reconcileConversationTurnAfterStreamTerminal } from '../reconcileConversationTurnAfterStreamTerminal';
import {
  classifyAuthoritativeTurnCompletion,
  classifyAuthoritativeTurnStart,
  resolveVerifiedAuthoritativeTurnStart,
} from '../authoritativeTurnLifecyclePolicy';
import { acpTurnReducer, initialAcpTurnState, isAcpTurnBusy } from './acpTurnState';

export const normalizeAcpSlashCommands = (commands: unknown): SlashCommandItem[] => {
  if (!Array.isArray(commands)) return [];

  return commands
    .filter((item): item is Record<string, unknown> => !!item && typeof item === 'object' && !Array.isArray(item))
    .flatMap((command) => {
      let name = '';
      if (typeof command.name === 'string') {
        name = command.name;
      } else if (typeof command.command === 'string') {
        name = command.command;
      }
      if (!name.trim()) return [];

      return [
        {
          name,
          description: command.description != null ? toDisplayText(command.description) : '',
          kind: 'template' as const,
          source: 'acp' as const,
          selectionBehavior: 'insert' as const,
        },
      ];
    });
};

export type UseAcpMessageReturn = {
  thought: ThoughtData;
  setThought: React.Dispatch<React.SetStateAction<ThoughtData>>;
  running: boolean;
  hasHydratedRunningState: boolean;
  acpStatus: 'connecting' | 'connected' | 'authenticated' | 'session_active' | 'disconnected' | 'error' | null;
  aiProcessing: boolean;
  setAiProcessing: (value: boolean) => void;
  markTurnAccepted: () => void;
  processingStartedAt?: number;
  resetState: () => void;
  confirmStopped: () => void;
  restoreRunningAfterStopFailure: () => void;
  getTurnStartGeneration: () => number;
  getTurnCompletionGeneration: () => number;
  tokenUsage: TokenUsageData | null;
  context_limit: number;
  hasThinkingMessage: boolean;
  slashCommands: SlashCommandItem[];
  fetchSlashCommands: () => void;
};

export const useAcpMessage = (conversation_id: ConversationId, options?: { skipWarmup?: boolean }): UseAcpMessageReturn => {
  const addOrUpdateMessage = useAddOrUpdateMessage();
  const [turnState, dispatchTurn] = useReducer(acpTurnReducer, initialAcpTurnState);
  const running = isAcpTurnBusy(turnState);
  const aiProcessing = running;
  const [hasHydratedRunningState, setHasHydratedRunningState] = useState(false);
  const [thought, setThought] = useState<ThoughtData>({
    description: '',
    subject: '',
  });
  const [acpStatus, setAcpStatus] = useState<
    'connecting' | 'connected' | 'authenticated' | 'session_active' | 'disconnected' | 'error' | null
  >(null);
  const [tokenUsage, setTokenUsage] = useState<TokenUsageData | null>(null);
  const [context_limit, setContextLimit] = useState<number>(0);
  const [slashCommands, setSlashCommands] = useState<SlashCommandItem[]>([]);

  // Correlate the conversation-scoped authoritative lifecycle. The stream can
  // contain multiple internal continuation msg_ids; only turn.started /
  // turn.completed carry the stable outer turn id.
  const rootTurnIdRef = useRef<MessageId | null>(null);
  const awaitingBackendTurnRef = useRef(false);
  const cancelledTurnIdsRef = useRef(new Set<MessageId>());
  const rejectUnannouncedStartRef = useRef(false);
  const verifyUnannouncedStartRuntimeRef = useRef(false);
  const turnLifecycleGenerationRef = useRef(0);
  const turnStartGenerationRef = useRef(0);
  const turnCompletionGenerationRef = useRef(0);
  const turnReconcileSequenceRef = useRef(0);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      turnLifecycleGenerationRef.current += 1;
      turnReconcileSequenceRef.current += 1;
    };
  }, []);

  // Track whether current turn has content output
  const hasContentInTurnRef = useRef(false);

  // Guard: after finish arrives, prevent auto-recover from setting running=true
  // until a new 'start' signal arrives for the next turn
  const turnFinishedRef = useRef(false);

  // Track whether current turn has a thinking message in the conversation
  const hasThinkingMessageRef = useRef(false);
  const [hasThinkingMessage, setHasThinkingMessage] = useState(false);
  const activeThinkingRef = useRef<{ msgId: MessageId; startedAt: number } | null>(null);

  // Track request trace state for displaying complete request lifecycle
  const requestTraceRef = useRef<{
    startTime: number;
    backend: string;
    model_id: string;
    session_mode?: string;
  } | null>(null);

  // Throttle thought updates to reduce render frequency
  const thoughtThrottleRef = useRef<{
    lastUpdate: number;
    pending: ThoughtData | null;
    timer: ReturnType<typeof setTimeout> | null;
  }>({ lastUpdate: 0, pending: null, timer: null });

  const throttledSetThought = useMemo(() => {
    const THROTTLE_MS = 50;
    return (data: ThoughtData) => {
      const now = Date.now();
      const ref = thoughtThrottleRef.current;
      if (now - ref.lastUpdate >= THROTTLE_MS) {
        ref.lastUpdate = now;
        ref.pending = null;
        if (ref.timer) {
          clearTimeout(ref.timer);
          ref.timer = null;
        }
        setThought(data);
      } else {
        ref.pending = data;
        if (!ref.timer) {
          ref.timer = setTimeout(
            () => {
              ref.lastUpdate = Date.now();
              ref.timer = null;
              if (ref.pending) {
                setThought(ref.pending);
                ref.pending = null;
              }
            },
            THROTTLE_MS - (now - ref.lastUpdate)
          );
        }
      }
    };
  }, []);

  // Clean up throttle timer
  useEffect(() => {
    return () => {
      if (thoughtThrottleRef.current.timer) {
        clearTimeout(thoughtThrottleRef.current.timer);
      }
    };
  }, []);

  const setAiProcessing = useCallback((value: boolean) => {
    turnLifecycleGenerationRef.current += 1;
    if (value) {
      turnStartGenerationRef.current += 1;
      awaitingBackendTurnRef.current = true;
      rootTurnIdRef.current = null;
      rejectUnannouncedStartRef.current = false;
      turnFinishedRef.current = false;
      dispatchTurn({ type: 'submit' });
      return;
    }

    awaitingBackendTurnRef.current = false;
    rootTurnIdRef.current = null;
    rejectUnannouncedStartRef.current = false;
    turnFinishedRef.current = true;
    dispatchTurn({ type: 'reset' });
  }, []);

  const settleCompletedTurn = useCallback(() => {
    turnLifecycleGenerationRef.current += 1;
    turnCompletionGenerationRef.current += 1;
    turnReconcileSequenceRef.current += 1;
    awaitingBackendTurnRef.current = false;
    rootTurnIdRef.current = null;
    rejectUnannouncedStartRef.current = false;
    turnFinishedRef.current = true;
    dispatchTurn({ type: 'finish' });
    setThought({ subject: '', description: '' });
    hasContentInTurnRef.current = false;
    hasThinkingMessageRef.current = false;
    activeThinkingRef.current = null;
    setHasThinkingMessage(false);
  }, []);

  const reconcileAfterStreamTerminal = useCallback(() => {
    const generation = turnLifecycleGenerationRef.current;
    const sequence = turnReconcileSequenceRef.current + 1;
    turnReconcileSequenceRef.current = sequence;
    void reconcileConversationTurnAfterStreamTerminal(
      conversation_id,
      () =>
        mountedRef.current &&
        turnLifecycleGenerationRef.current === generation &&
        turnReconcileSequenceRef.current === sequence,
      settleCompletedTurn
    );
  }, [conversation_id, settleCompletedTurn]);

  const markTurnAccepted = useCallback(
    () => {
      if (!awaitingBackendTurnRef.current || rejectUnannouncedStartRef.current) return;
      if (!verifyUnannouncedStartRuntimeRef.current) turnLifecycleGenerationRef.current += 1;
      rootTurnIdRef.current = null;
      awaitingBackendTurnRef.current = false;
      const generation = turnLifecycleGenerationRef.current;
      const sequence = turnReconcileSequenceRef.current + 1;
      turnReconcileSequenceRef.current = sequence;
      void reconcileConversationTurnAfterStreamTerminal(
        conversation_id,
        () =>
          mountedRef.current &&
          turnLifecycleGenerationRef.current === generation &&
          turnReconcileSequenceRef.current === sequence,
        settleCompletedTurn
      );
    },
    [conversation_id, settleCompletedTurn]
  );

  const completeActiveThinking = useCallback(
    (
      boundaryMessage: Pick<IResponseMessage, 'conversation_id' | 'created_at'>,
      completeOptions?: {
        duration?: number;
      }
    ) => {
      const activeThinking = activeThinkingRef.current;
      if (!activeThinking) return;

      const endTime = boundaryMessage.created_at ?? Date.now();
      const duration = completeOptions?.duration ?? Math.max(0, endTime - activeThinking.startedAt);

      addOrUpdateMessage({
        id: `${activeThinking.msgId}-thinking-done`,
        type: 'thinking',
        msg_id: activeThinking.msgId,
        conversation_id: boundaryMessage.conversation_id,
        position: 'left',
        created_at: endTime,
        content: {
          content: '',
          duration,
          status: 'done',
        },
      });

      activeThinkingRef.current = null;
    },
    [addOrUpdateMessage]
  );

  const handleResponseMessage = useCallback(
    (message: IResponseMessage) => {
      if (conversation_id !== message.conversation_id) {
        return;
      }

      if (message.type === 'skill_suggest' || message.type === 'cron_trigger') {
        return;
      }

      const shouldCompleteThinking =
        activeThinkingRef.current &&
        ![
          'thought',
          'thinking',
          'start',
          'request_trace',
          'acp_context_usage',
          'acp_model_info',
          'codex_model_info',
          'available_commands',
          'slash_commands_updated',
          'agent_status',
          'user_content',
          ACP_AGENT_MESSAGE_EVENT,
        ].includes(message.type);

      if (shouldCompleteThinking) {
        completeActiveThinking(message);
      }

      const transformedMessage = transformMessage(message);
      switch (message.type) {
        case 'thought':
          // Thought events are now handled by AcpAgentManager (converted to thinking messages)
          // Only auto-recover running state if turn hasn't finished
          if (!turnFinishedRef.current) {
            dispatchTurn({ type: 'activity' });
          }
          break;
        case 'thinking': {
          const thinkingData = message.data as { status?: string; duration?: number; duration_ms?: number };
          if (thinkingData?.status === 'done') {
            if (activeThinkingRef.current?.msgId === message.msg_id) {
              completeActiveThinking(message, {
                duration: thinkingData.duration ?? thinkingData.duration_ms,
              });
            }
            break;
          }

          // Only set running for active thinking, not for done signal
          if (!turnFinishedRef.current) {
            dispatchTurn({ type: 'thinking' });
          }
          if (!activeThinkingRef.current) {
            activeThinkingRef.current = {
              msgId: message.msg_id,
              startedAt: message.created_at ?? Date.now(),
            };
          } else if (activeThinkingRef.current.msgId !== message.msg_id) {
            activeThinkingRef.current = {
              msgId: message.msg_id,
              startedAt: message.created_at ?? Date.now(),
            };
          }
          hasThinkingMessageRef.current = true;
          setHasThinkingMessage(true);
          addOrUpdateMessage(transformedMessage);
          break;
        }
        case 'start':
          // responseStream start is not correlated with the conversation-owned
          // outer turn. turn.started/GET alone may open busy/rendering state,
          // so a late raw start cannot revive a known-root stopped turn.
          dispatchTurn({ type: 'rawStreamStarted' });
          break;
        case 'finish':
          {
            // Close stream rendering, but keep the backend-owned busy state
            // until turn.completed or an authoritative runtime read settles it.
            turnFinishedRef.current = true;
            setThought({ subject: '', description: '' });
            hasContentInTurnRef.current = false;
            hasThinkingMessageRef.current = false;
            activeThinkingRef.current = null;
            setHasThinkingMessage(false);
            // Log request completion
            if (requestTraceRef.current) {
              const duration = Date.now() - requestTraceRef.current.startTime;
              console.log(
                `%c[RequestTrace]%c FINISH | ${requestTraceRef.current.backend} → ${requestTraceRef.current.model_id} | ${duration}ms | ${new Date().toISOString()}`,
                'color: #52c41a; font-weight: bold',
                'color: inherit'
              );
              requestTraceRef.current = null;
            }
            reconcileAfterStreamTerminal();
          }
          break;
        case 'text':
        case 'content': {
          // First content token — AI has started responding, clear processing indicator
          if (!hasContentInTurnRef.current) {
            hasContentInTurnRef.current = true;
          }
          // Auto-recover running state only if turn hasn't finished
          if (!turnFinishedRef.current) {
            dispatchTurn({ type: 'content' });
          }
          // Clear thought when final answer arrives
          setThought({ subject: '', description: '' });
          addOrUpdateMessage(transformedMessage);
          break;
        }
        case 'agent_status': {
          // Update ACP/Agent status
          const agentData = message.data as {
            status?: 'connecting' | 'connected' | 'authenticated' | 'session_active' | 'disconnected' | 'error';
            backend?: string;
          };
          if (agentData?.status) {
            setAcpStatus(agentData.status);
            // Reset all loading states on error or disconnect so UI doesn't stay stuck
            if (['error', 'disconnected'].includes(agentData.status)) {
              turnFinishedRef.current = true;
              reconcileAfterStreamTerminal();
            }
          }
          addOrUpdateMessage(transformedMessage);
          break;
        }
        case 'user_content':
          addOrUpdateMessage(transformedMessage);
          break;
        case ACP_AGENT_MESSAGE_EVENT: {
          const tmMsg = message.data as import('@/common/chat/chatLib').TMessage;
          if (tmMsg && tmMsg.conversation_id === conversation_id) {
            if (tmMsg.type === 'text') {
              const raw = tmMsg.content as unknown;
              if (typeof raw === 'string') {
                try {
                  const parsed = JSON.parse(raw) as Record<string, unknown>;
                  if (typeof parsed.content === 'string') {
                    tmMsg.content = {
                      content: parsed.content,
                      ...normalizeWireAgentMessageMetadata(parsed),
                    };
                  }
                } catch {
                  /* keep original */
                }
              } else if (typeof raw === 'object' && raw !== null) {
                const obj = raw as Record<string, unknown>;
                const agentMetadata = normalizeWireAgentMessageMetadata(obj);
                if (agentMetadata.agentMessage && !obj.agentMessage) {
                  tmMsg.content = {
                    content: obj.content != null ? toDisplayText(obj.content) : '',
                    ...agentMetadata,
                  };
                }
              }
            }
            addOrUpdateMessage(tmMsg);
          }
          break;
        }
        case 'acp_permission':
          // Auto-recover running state only if turn hasn't finished
          if (!turnFinishedRef.current) {
            dispatchTurn({ type: 'permission' });
          }
          addOrUpdateMessage(transformedMessage);
          break;
        case 'acp_tool_call':
          if (!turnFinishedRef.current) {
            dispatchTurn({ type: 'tooling' });
          }
          addOrUpdateMessage(transformedMessage);
          break;
        case 'acp_model_info':
          // Model info updates are handled by AcpModelSelector, no action needed here
          break;
        case 'slash_commands_updated':
          // Slash commands became available (often during bootstrap when
          // agent_status events are suppressed). Update acpStatus so
          // useSlashCommands re-fetches.
          setAcpStatus((prev) => prev ?? 'session_active');
          break;
        case 'available_commands': {
          const cmdData = message.data as { commands?: AvailableCommand[] };
          if (cmdData?.commands && Array.isArray(cmdData.commands)) {
            setSlashCommands(normalizeAcpSlashCommands(cmdData.commands));
          }
          break;
        }
        case 'acp_context_usage': {
          const usageData = message.data as { used: number; size: number };
          if (usageData && typeof usageData.used === 'number') {
            setTokenUsage({ total_tokens: usageData.used });
            if (usageData.size > 0) {
              setContextLimit(usageData.size);
            }
          }
          break;
        }
        case 'request_trace':
          {
            const trace = message.data as Record<string, unknown>;
            requestTraceRef.current = {
              startTime: Number(trace.timestamp) || Date.now(),
              backend: String(trace.backend || 'unknown'),
              model_id: String(trace.model_id || 'unknown'),
              session_mode: trace.session_mode as string | undefined,
            };
            console.log(
              `%c[RequestTrace]%c START | ${trace.backend} → ${trace.model_id} | ${new Date().toISOString()}`,
              'color: #1890ff; font-weight: bold',
              'color: inherit',
              trace
            );
          }
          break;
        case 'error':
          // Error is terminal for stream rendering, not necessarily for the
          // backend turn handle. Authoritative completion lowers busy state.
          turnFinishedRef.current = true;
          activeThinkingRef.current = null;
          addOrUpdateMessage(transformedMessage);
          // Log request error
          if (requestTraceRef.current) {
            const duration = Date.now() - requestTraceRef.current.startTime;
            console.log(
              `%c[RequestTrace]%c ERROR | ${requestTraceRef.current.backend} → ${requestTraceRef.current.model_id} | ${duration}ms | ${new Date().toISOString()}`,
              'color: #ff4d4f; font-weight: bold',
              'color: inherit',
              message.data
            );
            requestTraceRef.current = null;
          }
          reconcileAfterStreamTerminal();
          break;
        default:
          // Auto-recover running state only if turn hasn't finished
          if (!turnFinishedRef.current) {
            dispatchTurn({ type: 'activity' });
          }
          addOrUpdateMessage(transformedMessage);
          break;
      }
    },
    [
      conversation_id,
      addOrUpdateMessage,
      completeActiveThinking,
      reconcileAfterStreamTerminal,
      throttledSetThought,
      setThought,
      setAcpStatus,
    ]
  );

  useEffect(() => {
    return ipcBridge.acpConversation.responseStream.on(handleResponseMessage);
  }, [handleResponseMessage]);

  useEffect(() => {
    let disposed = false;
    const unsubscribe = ipcBridge.conversation.turnStarted.on((event) => {
      if (conversation_id !== event.conversation_id) {
        return;
      }

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
        turnLifecycleGenerationRef.current += 1;
        awaitingBackendTurnRef.current = false;
        rootTurnIdRef.current = event.turn_id ?? null;
        rejectUnannouncedStartRef.current = false;
        verifyUnannouncedStartRuntimeRef.current = false;
        turnFinishedRef.current = false;
        hasContentInTurnRef.current = false;
        dispatchTurn({
          type: 'turnStarted',
          turnId: event.turn_id,
          processingStartedAt: event.runtime.processing_started_at,
        });
      };

      if (startAction === 'accept') {
        acceptStart();
        return;
      }

      const generation = turnLifecycleGenerationRef.current;
      void getConversationOrNull(conversation_id)
        .then((conversation) => {
          if (
            disposed ||
            turnLifecycleGenerationRef.current !== generation ||
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
          console.warn('[useAcpMessage] Failed to verify unannounced turn start:', error);
        });
    });
    return () => {
      disposed = true;
      unsubscribe();
    };
  }, [conversation_id]);

  useEffect(() => {
    let disposed = false;

    const unsubscribe = ipcBridge.conversation.turnCompleted.on((event) => {
      if (conversation_id !== event.conversation_id || event.runtime.is_processing) return;

      const rootTurnId = rootTurnIdRef.current;
      const awaitingBackendTurn = awaitingBackendTurnRef.current;
      const action = classifyAuthoritativeTurnCompletion({
        rootTurnId,
        completedTurnId: event.turn_id,
        awaitingBackendTurn,
      });
      if (action === 'settle') {
        settleCompletedTurn();
        return;
      }
      if (action === 'ignore') return;

      // Compatibility with older servers that omitted turn_id: verify the live
      // runtime before lowering UI state, and reject a start that races the GET.
      const observedRootTurnId = rootTurnId;
      const observedAwaitingBackendTurn = awaitingBackendTurn;
      const generation = turnLifecycleGenerationRef.current;
      const sequence = turnReconcileSequenceRef.current + 1;
      turnReconcileSequenceRef.current = sequence;
      void reconcileConversationTurnAfterStreamTerminal(
        conversation_id,
        () =>
          !disposed &&
          mountedRef.current &&
          turnLifecycleGenerationRef.current === generation &&
          turnReconcileSequenceRef.current === sequence &&
          rootTurnIdRef.current === observedRootTurnId &&
          awaitingBackendTurnRef.current === observedAwaitingBackendTurn,
        settleCompletedTurn
      );
    });

    return () => {
      disposed = true;
      unsubscribe();
    };
  }, [conversation_id, settleCompletedTurn]);

  // Reset state when conversation changes and restore actual running status
  useEffect(() => {
    let cancelled = false;

    setThought({ subject: '', description: '' });
    setAcpStatus(null);
    setTokenUsage(null);
    setContextLimit(0);
    setSlashCommands([]);
    hasContentInTurnRef.current = false;
    turnLifecycleGenerationRef.current += 1;
    const hydrationGeneration = turnLifecycleGenerationRef.current;
    turnFinishedRef.current = false;
    awaitingBackendTurnRef.current = false;
    rootTurnIdRef.current = null;
    cancelledTurnIdsRef.current.clear();
    rejectUnannouncedStartRef.current = false;
    verifyUnannouncedStartRuntimeRef.current = false;
    hasThinkingMessageRef.current = false;
    activeThinkingRef.current = null;
    setHasThinkingMessage(false);
    setHasHydratedRunningState(false);

    dispatchTurn({ type: 'reset' });

    void getConversationOrNull(conversation_id)
      .then((res) => {
        if (cancelled) {
          return;
        }
        if (turnLifecycleGenerationRef.current !== hydrationGeneration) {
          setHasHydratedRunningState(true);
          return;
        }

        if (!res) {
          dispatchTurn({ type: 'hydrate', isRunning: false });
          setHasHydratedRunningState(true);
          return;
        }
        const isRunning = isConversationProcessing(res);
        dispatchTurn({
          type: 'hydrate',
          isRunning,
          processingStartedAt: res.runtime?.processing_started_at,
        });
        setHasHydratedRunningState(true);

        // Restore persisted context usage data
        if (res.type === 'acp' && res.extra?.last_token_usage) {
          const { last_token_usage, last_context_limit } = res.extra;
          if (last_token_usage.total_tokens > 0) {
            setTokenUsage(last_token_usage);
          }
          if (last_context_limit && last_context_limit > 0) {
            setContextLimit(last_context_limit);
          }
        }
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        if (turnLifecycleGenerationRef.current !== hydrationGeneration) {
          setHasHydratedRunningState(true);
          return;
        }
        dispatchTurn({ type: 'hydrate', isRunning: false });
        setHasHydratedRunningState(true);

        if (error instanceof TypeError && error.message.includes('Failed to fetch')) {
          console.warn('[useAcpMessage] Failed to hydrate conversation state:', error);
          return;
        }

        throw error;
      });

    return () => {
      cancelled = true;
    };
  }, [conversation_id]);

  // Fetch slash commands via HTTP after warmup completes.
  // WebSocket push of available_commands arrives during warmup when no
  // StreamRelay is listening, so the initial load must come from HTTP.
  // Mirrors the nomi pattern: warmup first, then fetch.
  // Some collaboration hosts defer warmup to first user input.
  useEffect(() => {
    if (options?.skipWarmup) return;
    let cancelled = false;
    void warmupConversation(conversation_id)
      .then(() => {
        if (cancelled) return;
        return ipcBridge.conversation.getSlashCommands.invoke({ conversation_id });
      })
      .then((result) => {
        if (cancelled) return;
        if (!result || !Array.isArray(result) || result.length === 0) return;
        setSlashCommands(normalizeAcpSlashCommands(result));
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [conversation_id, options?.skipWarmup]);

  const resetState = useCallback(() => {
    turnLifecycleGenerationRef.current += 1;
    const rootTurnId = rootTurnIdRef.current;
    if (rootTurnId) {
      const cancelled = cancelledTurnIdsRef.current;
      cancelled.add(rootTurnId);
      // Bound tombstones for very long-lived conversations.
      if (cancelled.size > 32) {
        const oldest = cancelled.values().next().value;
        if (oldest) cancelled.delete(oldest);
      }
    }
    awaitingBackendTurnRef.current = false;
    rejectUnannouncedStartRef.current = true;
    verifyUnannouncedStartRuntimeRef.current = rootTurnId === null;
    turnFinishedRef.current = true;
    dispatchTurn({ type: 'reset' });
    setThought({ subject: '', description: '' });
    hasContentInTurnRef.current = false;
    hasThinkingMessageRef.current = false;
    activeThinkingRef.current = null;
    setHasThinkingMessage(false);
  }, []);

  const restoreRunningAfterStopFailure = useCallback(() => {
    turnLifecycleGenerationRef.current += 1;
    const rootTurnId = rootTurnIdRef.current;
    if (rootTurnId) cancelledTurnIdsRef.current.delete(rootTurnId);
    awaitingBackendTurnRef.current = false;
    rejectUnannouncedStartRef.current = false;
    verifyUnannouncedStartRuntimeRef.current = false;
    turnFinishedRef.current = false;
    dispatchTurn({ type: 'hydrate', isRunning: true });
    const generation = turnLifecycleGenerationRef.current;
    const sequence = turnReconcileSequenceRef.current + 1;
    turnReconcileSequenceRef.current = sequence;
    void reconcileConversationTurnAfterStreamTerminal(
      conversation_id,
      () =>
        mountedRef.current &&
        turnLifecycleGenerationRef.current === generation &&
        turnReconcileSequenceRef.current === sequence,
      settleCompletedTurn
    );
  }, [conversation_id, settleCompletedTurn]);

  const confirmStopped = useCallback(() => {
    turnLifecycleGenerationRef.current += 1;
    rootTurnIdRef.current = null;
    awaitingBackendTurnRef.current = false;
    rejectUnannouncedStartRef.current = false;
    turnFinishedRef.current = true;
    dispatchTurn({ type: 'reset' });
  }, []);

  const getTurnStartGeneration = useCallback(() => turnStartGenerationRef.current, []);
  const getTurnCompletionGeneration = useCallback(() => turnCompletionGenerationRef.current, []);

  const fetchSlashCommands = useCallback(() => {
    void ipcBridge.conversation.getSlashCommands
      .invoke({ conversation_id })
      .then((result) => {
        if (!result || !Array.isArray(result) || result.length === 0) return;
        setSlashCommands(normalizeAcpSlashCommands(result));
      })
      .catch(() => {});
  }, [conversation_id]);

  return {
    thought,
    setThought,
    running,
    hasHydratedRunningState,
    acpStatus,
    aiProcessing,
    setAiProcessing,
    markTurnAccepted,
    processingStartedAt: turnState.processingStartedAt,
    resetState,
    confirmStopped,
    restoreRunningAfterStopFailure,
    getTurnStartGeneration,
    getTurnCompletionGeneration,
    tokenUsage,
    context_limit,
    hasThinkingMessage,
    slashCommands,
    fetchSlashCommands,
  };
};
