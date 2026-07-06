/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IConversationMcpStatus } from '@/common/config/storage';
import { ConversationProvider } from '@/renderer/hooks/context/ConversationContext';
import FlexFullContainer from '@renderer/components/layout/FlexFullContainer';
import MessageList from '@renderer/pages/conversation/Messages/MessageList';
import { ConversationArtifactProvider } from '@renderer/pages/conversation/Messages/artifacts';
import {
  MessageListLoadingProvider,
  MessageListProvider,
  useAddOrUpdateMessage,
  useMessageLstCache,
} from '@renderer/pages/conversation/Messages/hooks';
import { usePendingConfirmationsRecovery } from '@renderer/pages/conversation/Messages/usePendingConfirmationsRecovery';
import { useAutoTitle } from '@/renderer/hooks/chat/useAutoTitle';
import HOC from '@renderer/utils/ui/HOC';
import React from 'react';
import AcpE2EStreamInjector from './AcpE2EStreamInjector';
import AcpSendBox from './AcpSendBox';
import { useAcpInitialMessage } from './useAcpInitialMessage';
import { useAcpMessage } from './useAcpMessage';

const AcpChat: React.FC<{
  conversation_id: number;
  workspace?: string;
  backend: string;
  initialModelId?: string;
  session_mode?: string;
  agent_name?: string;
  cron_job_id?: string;
  hideSendBox?: boolean;
  emptySlot?: React.ReactNode;
  loadedSkills?: string[];
  loadedMcpServers?: string[];
  loadedMcpStatuses?: IConversationMcpStatus[];
}> = ({
  conversation_id,
  workspace,
  backend,
  initialModelId,
  session_mode,
  agent_name,
  cron_job_id,
  hideSendBox,
  emptySlot,
  loadedSkills,
  loadedMcpServers,
  loadedMcpStatuses,
}) => {
  useMessageLstCache(conversation_id);
  usePendingConfirmationsRecovery(conversation_id);
  const { checkAndUpdateTitle } = useAutoTitle();
  const addOrUpdateMessage = useAddOrUpdateMessage();
  const messageState = useAcpMessage(conversation_id, { skipWarmup: false });
  useAcpInitialMessage({
    conversation_id,
    backend,
    workspacePath: workspace,
    setAiProcessing: messageState.setAiProcessing,
    checkAndUpdateTitle,
    addOrUpdateMessage,
  });

  return (
    <ConversationProvider
      value={{
        conversation_id: conversation_id,
        workspace,
        type: 'acp',
        cron_job_id,
        hideSendBox,
        isProcessing: messageState.running,
        loadedSkills,
        loadedMcpServers,
        loadedMcpStatuses,
      }}
    >
      <ConversationArtifactProvider conversation_id={conversation_id}>
        <div className='flex-1 flex flex-col px-20px min-h-0'>
          <FlexFullContainer>
            <MessageList className='flex-1' emptySlot={emptySlot} />
          </FlexFullContainer>
          <AcpE2EStreamInjector conversationId={conversation_id} />
          {!hideSendBox && (
            <AcpSendBox
              conversation_id={conversation_id}
              backend={backend}
              initialModelId={initialModelId}
              session_mode={session_mode}
              agent_name={agent_name}
              workspacePath={workspace}
              messageState={messageState}
            ></AcpSendBox>
          )}
        </div>
      </ConversationArtifactProvider>
    </ConversationProvider>
  );
};

export default HOC.Wrapper(MessageListProvider, MessageListLoadingProvider)(AcpChat);
