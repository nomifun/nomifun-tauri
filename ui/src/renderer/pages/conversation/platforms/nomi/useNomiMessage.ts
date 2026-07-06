/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import { transformMessage } from '@/common/chat/chatLib';
import type { IResponseMessage } from '@/common/adapter/ipcBridge';
import type { TChatConversation, TokenUsageData } from '@/common/config/storage';
import { prefixedId, uuid } from '@/common/utils';
import { useAddOrUpdateMessage } from '@/renderer/pages/conversation/Messages/hooks';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { isConversationProcessing } from '@/renderer/pages/conversation/utils/conversationRuntime';
import { emitter } from '@/renderer/utils/emitter';
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react';
import type { ThoughtData } from '../thoughtTypes';
import { processLocalCronResponse } from './localCronCommands';
import { initialNomiTurnState, isTurnRunning, nomiTurnReducer } from './nomiTurnState';

export const useNomiMessage = (
  conversation_id: number,
  options?: {
    onError?: (message: IResponseMessage) => void;
    onConfigChanged?: (capabilities: Record<string, unknown>) => void;
  }
) => {
  const onError = options?.onError;
  const onConfigChanged = options?.onConfigChanged;
  const onConfigChangedRef = useRef(onConfigChanged);
  const addOrUpdateMessage = useAddOrUpdateMessage();
  // Single source of truth for the turn's activity state (design §3.2): a pure
  // reducer over lifecycle events replaces three hand-synced booleans.
  const [turnState, dispatchTurn] = useReducer(nomiTurnReducer, initialNomiTurnState);
  const [hasHydratedRunningState, setHasHydratedRunningState] = useState(false);
  const [thought, setThought] = useState<ThoughtData>({
    description: '',
    subject: '',
  });
  const [tokenUsage, setTokenUsage] = useState<TokenUsageData | null>(null);
  // Current active message ID to filter out events from old requests (prevents aborted request events from interfering with new ones)
  const activeMsgIdRef = useRef<string | null>(null);
  const messageBufferRef = useRef(new Map<string, string>());
  const processedCronMsgIdsRef = useRef(new Set<string>());

  // Mirror the reducer state into a ref so the (non-resubscribing) stream
  // closure can read the current turn state without being a dependency.
  const turnStateRef = useRef(turnState);
  useEffect(() => {
    turnStateRef.current = turnState;
  }, [turnState]);

  useEffect(() => {
    onConfigChangedRef.current = onConfigChanged;
  }, [onConfigChanged]);

  // Throttle thought updates to reduce render frequency
  const thoughtThrottleRef = useRef<{
    lastUpdate: number;
    pending: ThoughtData | null;
    timer: ReturnType<typeof setTimeout> | null;
  }>({ lastUpdate: 0, pending: null, timer: null });

  const throttledSetThought = useMemo(() => {
    const THROTTLE_MS = 50; // 50ms throttle interval
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

  // Cleanup throttle timer
  useEffect(() => {
    return () => {
      if (thoughtThrottleRef.current.timer) {
        clearTimeout(thoughtThrottleRef.current.timer);
      }
    };
  }, []);

  // Combined running state: waiting for response OR stream is running OR tools are active
  const running = isTurnRunning(turnState);

  // Set current active message ID
  const setActiveMsgId = useCallback((msgId: string | null) => {
    activeMsgIdRef.current = msgId;
  }, []);

  const processCompletedAssistantMessage = useCallback(
    async (msgId: string) => {
      if (!msgId || processedCronMsgIdsRef.current.has(msgId)) {
        return;
      }

      const rawContent = messageBufferRef.current.get(msgId) ?? '';
      if (!rawContent.trim()) {
        return;
      }

      processedCronMsgIdsRef.current.add(msgId);

      try {
        const result = await processLocalCronResponse(conversation_id, rawContent);
        if (result.displayContent !== undefined && result.displayContent !== rawContent) {
          addOrUpdateMessage({
            id: uuid(),
            msg_id: msgId,
            type: 'text',
            position: 'left',
            conversation_id,
            created_at: Date.now(),
            content: {
              content: result.displayContent,
              replace: true,
            },
          });
        }

        for (const response of result.systemResponses) {
          addOrUpdateMessage(
            {
              id: prefixedId('msg'),
              // Local-only placeholder key (never sent to the backend): the
              // `cron-local-` prefix marks locally generated cron system
              // responses for de-dup, so it keeps the legacy uuid() format.
              msg_id: `cron-local-${uuid()}`,
              type: 'tips',
              position: 'center',
              conversation_id,
              created_at: Date.now(),
              content: {
                content: response,
                type: response.startsWith('❌') ? 'error' : 'success',
              },
            },
            true
          );
        }
      } catch {
        processedCronMsgIdsRef.current.delete(msgId);
      }
    },
    [addOrUpdateMessage, conversation_id]
  );

  useEffect(() => {
    return ipcBridge.conversation.responseStream.on((message) => {
      if (conversation_id !== message.conversation_id) {
        return;
      }

      // Filter out events not belonging to current active request (prevents aborted events from interfering)
      // Note: only filter out thought and start messages, other messages must be rendered
      if (activeMsgIdRef.current && message.msg_id && message.msg_id !== activeMsgIdRef.current) {
        if (message.type === 'thought') {
          return;
        }
      }

      if ((message.type === 'content' || message.type === 'text') && message.msg_id) {
        const payload = message.data;
        const chunk =
          typeof payload === 'string'
            ? payload
            : typeof payload === 'object' &&
                payload !== null &&
                'content' in payload &&
                typeof (payload as { content?: unknown }).content === 'string'
              ? ((payload as { content: string }).content ?? '')
              : '';

        if (chunk) {
          const previous = messageBufferRef.current.get(message.msg_id) ?? '';
          messageBufferRef.current.set(message.msg_id, previous + chunk);
        }
      }

      switch (message.type) {
        case 'thought':
          dispatchTurn({ type: 'activity' });
          throttledSetThought(message.data as ThoughtData);
          break;
        case 'start':
          dispatchTurn({ type: 'activity' });
          // Don't reset waitingResponse here - let tool completion flow handle it
          break;
        case 'turn_completed':
          {
            // Phase 3 observability: the engine emits one turn_completed per turn
            // carrying real aggregate metrics. This is the genuine source of token
            // usage for nomi turns (the finish event has never carried usage) —
            // it updates the send-box metrics chip and persists for rehydration.
            const metrics = message.data as
              | {
                  elapsed_ms?: number;
                  input_tokens?: number;
                  output_tokens?: number;
                  cache_creation_tokens?: number;
                  cache_read_tokens?: number;
                  context_tokens?: number;
                  context_window?: number;
                }
              | undefined;
            if (metrics && typeof metrics === 'object') {
              const inputTokens = metrics.input_tokens || 0;
              const outputTokens = metrics.output_tokens || 0;
              const newTokenUsage: TokenUsageData = {
                total_tokens: inputTokens + outputTokens,
                input_tokens: metrics.input_tokens,
                output_tokens: metrics.output_tokens,
                cache_creation_tokens: metrics.cache_creation_tokens,
                cache_read_tokens: metrics.cache_read_tokens,
                elapsed_ms: metrics.elapsed_ms,
                context_tokens: metrics.context_tokens,
                context_window: metrics.context_window,
              };
              setTokenUsage(newTokenUsage);
              emitter.emit('nomi.usage.updated', { conversation_id, tokenUsage: newTokenUsage });
              void ipcBridge.conversation.update.invoke({
                id: conversation_id,
                updates: {
                  extra: { last_token_usage: newTokenUsage } as TChatConversation['extra'],
                },
                merge_extra: true,
              });
            }
          }
          break;
        case 'finish':
          {
            dispatchTurn({ type: 'finish' });
            setThought({ subject: '', description: '' });
            if (message.msg_id) {
              void processCompletedAssistantMessage(message.msg_id);
            }
          }
          break;
        case 'tool_group':
          {
            // Check if any tools are executing or awaiting confirmation
            const tools = message.data as Array<{ status: string; name?: string; description?: string }>;
            const activeStatuses = new Set(['Executing', 'Confirming', 'Pending']);
            const hasActive = tools.some((tool) => activeStatuses.has(tool.status));
            dispatchTurn({ type: 'toolGroup', hasActive, hasAny: tools.length > 0 });

            // If tools are awaiting confirmation, update thought hint
            const confirmingTool = tools.find((tool) => tool.status === 'Confirming');
            if (confirmingTool) {
              setThought({
                subject: 'Awaiting Confirmation',
                // Prefer the contextual description (file/command/pattern) over the
                // bare tool name so the status reads e.g. "edit src/auth.ts".
                description: confirmingTool.description || confirmingTool.name || 'Tool execution',
              });
            } else if (hasActive) {
              const executingTool = tools.find((tool) => tool.status === 'Executing');
              if (executingTool) {
                setThought({
                  subject: 'Executing',
                  description: executingTool.description || executingTool.name || 'Tool',
                });
              }
            } else if (!turnStateRef.current.streamRunning) {
              // All tools completed and stream stopped, clear thought
              setThought({ subject: '', description: '' });
            }

            // Continue passing message to message list update
            addOrUpdateMessage(transformMessage(message));
          }
          break;
        case 'permission':
        case 'acp_permission':
          dispatchTurn({ type: 'activity' });
          // Backend nomi emits wire type 'acp_permission' but the payload is
          // Confirmation-shaped (legacy), which matches MessagePermission, not
          // MessageAcpPermission. Re-tag so transformMessage routes it correctly.
          addOrUpdateMessage(transformMessage({ ...message, type: 'permission' }));
          break;
        case 'config_changed':
          onConfigChangedRef.current?.(message.data as Record<string, unknown>);
          break;
        default: {
          if (message.type === 'error') {
            dispatchTurn({ type: 'error' });
            setThought({ subject: '', description: '' });
            onError?.(message as IResponseMessage);
          } else if (message.type === 'content') {
            // Actual assistant content: running, no longer waiting for the model.
            dispatchTurn({ type: 'content' });
          } else {
            // Any other non-error output: keep the turn marked running (handles
            // events that arrive after a premature finish).
            dispatchTurn({ type: 'activity' });
          }
          // Backend handles persistence, Frontend only updates UI
          addOrUpdateMessage(transformMessage(message));
          break;
        }
      }
    });
    // Note: turn state is read via turnStateRef to avoid re-subscription
  }, [conversation_id, addOrUpdateMessage, onError, processCompletedAssistantMessage]);

  useEffect(() => {
    let cancelled = false;

    // Clear turn state on conversation switch so a previous conversation's
    // running state cannot bleed into this one; the raise-only `hydrate` below
    // then merges the backend status with any send that races the async query.
    dispatchTurn({ type: 'reset' });
    setThought({ subject: '', description: '' });
    setTokenUsage(null);
    setHasHydratedRunningState(false);

    // Check actual conversation status from backend before resetting all running states
    // to avoid flicker when switching to a running conversation
    void getConversationOrNull(conversation_id).then((res) => {
      if (cancelled) {
        return;
      }

      if (!res) {
        // No conversation record — already reset at effect start; just mark hydrated.
        setHasHydratedRunningState(true);
        return;
      }
      const isRunning = isConversationProcessing(res);
      // A send issued between this conversation mounting and this async query
      // resolving has already raised the spinner (executeCommand →
      // setWaitingResponse(true)). The query was fired BEFORE that send, so its
      // is_processing=false is stale — must NOT clobber a locally-raised running
      // state, or a brand-new conversation's first message shows no "正在处理"
      // indicator until the first live stream event arrives. `hydrate` is
      // raise-only, so it ORs the backend status onto whatever is already set.
      dispatchTurn({ type: 'hydrate', isRunning });
      // Load persisted token usage stats
      if (res.type === 'nomi' && res.extra?.last_token_usage) {
        const { last_token_usage } = res.extra;
        if (last_token_usage.total_tokens > 0) {
          setTokenUsage(last_token_usage);
        }
      }
      setHasHydratedRunningState(true);
    });

    return () => {
      cancelled = true;
    };
  }, [conversation_id]);

  const resetState = useCallback(() => {
    dispatchTurn({ type: 'reset' });
    setThought({ subject: '', description: '' });
    // Clear active message ID to prevent filtering events from new messages after stop
    activeMsgIdRef.current = null;
  }, []);

  // External setter used by the send box to raise the spinner on submit.
  const setWaitingResponse = useCallback((value: boolean) => {
    dispatchTurn({ type: 'setWaiting', value });
  }, []);

  return {
    thought,
    setThought,
    running,
    hasHydratedRunningState,
    tokenUsage,
    setActiveMsgId,
    setWaitingResponse,
    resetState,
  };
};

export type NomiMessageRuntime = ReturnType<typeof useNomiMessage>;
