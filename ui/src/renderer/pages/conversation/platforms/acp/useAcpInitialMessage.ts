/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import { conversationTarget, type ConversationId, type MessageId } from '@/common/types/ids';
import { sessionStorageKey } from '@/common/utils/browserStorageKey';

import { ipcBridge } from '@/common';
import type { TMessage } from '@/common/chat/chatLib';
import { parseError, uuid } from '@/common/utils';
import { emitter } from '@/renderer/utils/emitter';
import { buildDisplayMessage } from '@/renderer/utils/file/messageFiles';
import { Message } from '@arco-design/web-react';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import {
  claimInitialMessageDelivery,
  completeInitialMessageDelivery,
  handleInitialMessageDeliveryFailure,
  readAuthorizedInitialMessageDelivery,
  releaseInitialMessageDelivery,
} from '../initialMessageDelivery';
import { classifyPublicMessageDelivery } from '../publicMessageDelivery';
import { getConversationRuntimeWorkspaceErrorMessage } from '../../utils/conversationCreateError';

type UseAcpInitialMessageParams = {
  conversation_id: ConversationId;
  backend: string;
  workspacePath?: string;
  enabled?: boolean;
  setAiProcessing: (value: boolean) => void;
  markTurnAccepted: (requestMessageId?: MessageId) => void;
  reconcilePublicDeliveryReplay: (completed: boolean) => void;
  checkAndUpdateTitle: (conversation_id: ConversationId, input: string) => void;
  addOrUpdateMessage: (message: TMessage, prepend?: boolean) => void;
};

/**
 * Side-effect-only hook that checks sessionStorage for an initial message
 * and sends it when the ACP conversation first mounts.
 */
export const useAcpInitialMessage = ({
  conversation_id,
  backend,
  workspacePath,
  enabled = true,
  setAiProcessing,
  markTurnAccepted,
  reconcilePublicDeliveryReplay,
  checkAndUpdateTitle,
  addOrUpdateMessage,
}: UseAcpInitialMessageParams): void => {
  const { t } = useTranslation();

  useEffect(() => {
    if (!enabled) return;

    const storageKey = sessionStorageKey('initial-message-acp', conversationTarget(conversation_id));
    if (!sessionStorage.getItem(storageKey) || !claimInitialMessageDelivery(storageKey)) return;

    const sendInitialMessage = async () => {
      let attemptedIdempotencyKey: string | null = null;
      try {
        const initialMessage = await readAuthorizedInitialMessageDelivery(
          sessionStorage,
          storageKey,
          conversation_id
        );
        if (!initialMessage) {
          releaseInitialMessageDelivery(storageKey);
          return;
        }
        const { input, files, idempotency_key } = initialMessage;
        attemptedIdempotencyKey = idempotency_key;
        const displayMessage = buildDisplayMessage(input, files, workspacePath || '');

        // POST first to obtain the server-assigned msg_id, then render the
        // optimistic user bubble with that canonical id. Doing it in this
        // order prevents `useMessageLstCache` from treating the optimistic
        // row as a separate "streaming-only" entry when the DB load races
        // with sendMessage — which previously produced two duplicated user
        // bubbles on the first conversation render.
        const delivery = await ipcBridge.acpConversation.sendMessage.invoke({
          input: displayMessage,
          conversation_id: conversation_id,
          files,
          idempotency_key,
          initial_only: true,
        });
        const { msg_id } = delivery;
        // The bridge only resolves for a successful HTTP response. Consume the
        // handoff now; all transport failures retain it for a stable-key retry.
        completeInitialMessageDelivery(sessionStorage, storageKey, idempotency_key);
        const disposition = classifyPublicMessageDelivery(delivery);
        if (disposition === 'fresh') {
          setAiProcessing(true);
          void checkAndUpdateTitle(conversation_id, input);
          markTurnAccepted(msg_id);

        // Use add=false (compose mode) so composeMessageWithIndex can de-dup
        // by msg_id — this prevents a duplicate bubble if useMessageLstCache
        // already inserted the DB row for this same msg_id.
        addOrUpdateMessage({
          id: uuid(),
          msg_id,
          type: 'text',
          position: 'right',
          conversation_id,
          content: { content: displayMessage },
          created_at: Date.now(),
        });
        } else {
          reconcilePublicDeliveryReplay(delivery.completed);
        }

        // Initial message sent successfully
        emitter.emit('chat.history.refresh');
      } catch (error) {
        handleInitialMessageDeliveryFailure(
          sessionStorage,
          storageKey,
          attemptedIdempotencyKey,
          error
        );
        const errorMessageText =
          getConversationRuntimeWorkspaceErrorMessage(error, t) || parseError(error) || t('common.unknownError');
        console.error('[useAcpInitialMessage] Error sending initial message:', error);
        console.error('[useAcpInitialMessage] Error details:', {
          name: (error as Error)?.name,
          message: errorMessageText,
          conversation_id,
        });

        // The backend owns durable transcript errors and their canonical
        // identity. A POST failure that never produced a server message is
        // transient UI feedback, not a synthetic chat row that history
        // reconciliation could later duplicate or move into another turn.
        Message.error({ content: errorMessageText, duration: 6000 });
        setAiProcessing(false); // Stop loading state on error
      }
    };

    sendInitialMessage().catch((error) => {
      console.error('Failed to send initial message:', error);
    });
  }, [
    addOrUpdateMessage,
    backend,
    checkAndUpdateTitle,
    conversation_id,
    enabled,
    markTurnAccepted,
    reconcilePublicDeliveryReplay,
    setAiProcessing,
    t,
    workspacePath,
  ]);
};
