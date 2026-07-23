/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import { parseMessageId, type ConversationId } from '@/common/types/ids';
import { uuidv7 } from '@/common/utils/uuidv7';
import { uuid } from '@/common/utils';

import type { TMessage } from '@/common/chat/chatLib';
import { useAddOrUpdateMessage } from '@/renderer/pages/conversation/Messages/hooks';
import React, { useEffect } from 'react';

const STREAM_TICK_MS = 35;
const ENABLED_CONVERSATION_KEY = 'nomifun:e2e-message-stream-conversation-id';

type RunScenarioOptions = {
  historyPairs?: number;
  lines?: number;
  seedHistoryOnly?: boolean;
};

type StreamController = {
  runScenario: (options?: RunScenarioOptions) => Promise<void>;
};

type StreamRegistry = {
  controllers: Record<string, StreamController>;
};

declare global {
  interface Window {
    __NOMIFUN_E2E_MESSAGE_STREAM__?: StreamRegistry;
  }
}

const createSeedMessages = (conversationId: ConversationId, historyPairs: number): TMessage[] => {
  const baseCreatedAt = Date.now() - 100_000;
  const messages: TMessage[] = [];

  for (let index = 0; index < historyPairs; index += 1) {
    const userMessageId = parseMessageId(uuidv7());
    messages.push({
      id: `e2e-seed-user-${index}`,
      msg_id: userMessageId,
      conversation_id: conversationId,
      type: 'text',
      position: 'right',
      created_at: baseCreatedAt + index * 2,
      content: {
        content: `User seed message ${index + 1}: keep the list tall enough to overflow.`,
      },
    });

    const assistantMessageId = parseMessageId(uuidv7());
    messages.push({
      id: `e2e-seed-assistant-${index}`,
      msg_id: assistantMessageId,
      conversation_id: conversationId,
      type: 'text',
      position: 'left',
      created_at: baseCreatedAt + index * 2 + 1,
      content: {
        content: `Assistant seed reply ${index + 1}: this is stable history used to create a realistic scroll range.`,
      },
    });
  }

  const finalUserMessageId = parseMessageId(uuidv7());
  messages.push({
    id: 'e2e-seed-user-final',
    msg_id: finalUserMessageId,
    conversation_id: conversationId,
    type: 'text',
    position: 'right',
    created_at: baseCreatedAt + historyPairs * 2 + 1,
    content: {
      content: 'Please stream a long reply line by line so the message list keeps growing.',
    },
  });

  return messages;
};

const createStreamChunks = (lines: number): string[] => {
  return Array.from(
    { length: lines },
    (_, index) =>
      `${index + 1}. Streamed line ${index + 1} keeps extending the assistant reply to stress-test bottom-follow scrolling.\n`
  );
};

const AcpE2EStreamInjector: React.FC<{ conversationId: ConversationId }> = ({ conversationId }) => {
  const addOrUpdateMessage = useAddOrUpdateMessage();

  useEffect(() => {
    const enabledConversationId =
      typeof window !== 'undefined' ? window.sessionStorage.getItem(ENABLED_CONVERSATION_KEY) : null;
    if (enabledConversationId !== conversationId) {
      return;
    }

    const registry = (window.__NOMIFUN_E2E_MESSAGE_STREAM__ ??= { controllers: {} });
    const registryKey = conversationId;

    registry.controllers[registryKey] = {
      runScenario: async (options?: RunScenarioOptions) => {
        const historyPairs = options?.historyPairs ?? 18;
        const lines = options?.lines ?? 160;
        const streamMsgId = parseMessageId(uuidv7());

        if (historyPairs > 0) {
          createSeedMessages(conversationId, historyPairs).forEach((message) => addOrUpdateMessage(message, true));
        }

        if (options?.seedHistoryOnly) {
          return;
        }

        const chunks = createStreamChunks(lines);
        await new Promise<void>((resolve) => {
          let chunkIndex = 0;

          const pushNextChunk = () => {
            if (chunkIndex >= chunks.length) {
              resolve();
              return;
            }

            addOrUpdateMessage({
              id: uuid(),
              msg_id: streamMsgId,
              conversation_id: conversationId,
              type: 'text',
              position: 'left',
              created_at: Date.now() + chunkIndex,
              content: {
                content: chunks[chunkIndex],
              },
            });
            chunkIndex += 1;
            window.setTimeout(pushNextChunk, STREAM_TICK_MS);
          };

          pushNextChunk();
        });
      },
    };

    return () => {
      if (window.__NOMIFUN_E2E_MESSAGE_STREAM__) {
        delete window.__NOMIFUN_E2E_MESSAGE_STREAM__.controllers[registryKey];
      }
    };
  }, [addOrUpdateMessage, conversationId]);

  return null;
};

export default AcpE2EStreamInjector;
