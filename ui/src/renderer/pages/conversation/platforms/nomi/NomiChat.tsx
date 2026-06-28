/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IConversationMcpStatus } from '@/common/config/storage';
import type { ConversationContextValue } from '@/renderer/hooks/context/ConversationContext';
import { ConversationProvider } from '@/renderer/hooks/context/ConversationContext';
import FlexFullContainer from '@renderer/components/layout/FlexFullContainer';
import MessageList from '@renderer/pages/conversation/Messages/MessageList';
import PinnedPlan from '@renderer/pages/conversation/Messages/components/PinnedPlan';
import { ConversationArtifactProvider } from '@renderer/pages/conversation/Messages/artifacts';
import {
  MessageListLoadingProvider,
  MessageListProvider,
  useMessageLstCache,
} from '@renderer/pages/conversation/Messages/hooks';
import { usePendingConfirmationsRecovery } from '@renderer/pages/conversation/Messages/usePendingConfirmationsRecovery';
import HOC from '@renderer/utils/ui/HOC';
import React, { useEffect, useMemo } from 'react';
import LocalImageView from '@renderer/components/media/LocalImageView';
import NomiSendBox from './NomiSendBox';
import type { NomiModelSelection } from './useNomiModelSelection';

const NomiChat: React.FC<{
  conversation_id: number;
  workspace: string;
  modelSelection: NomiModelSelection;
  session_mode?: string;
  cron_job_id?: string;
  hideSendBox?: boolean;
  emptySlot?: React.ReactNode;
  loadedSkills?: string[];
  loadedMcpServers?: string[];
  loadedMcpStatuses?: IConversationMcpStatus[];
  agent_name?: string;
  /** Hide the permission/agent-mode selector in the send box (locked surfaces). */
  hideModeSelector?: boolean;
}> = ({
  conversation_id,
  workspace,
  modelSelection,
  session_mode,
  cron_job_id,
  hideSendBox,
  emptySlot,
  loadedSkills,
  loadedMcpServers,
  loadedMcpStatuses,
  agent_name,
  hideModeSelector,
}) => {
  // Windowed history: load only the newest page on mount + lazily prepend older
  // pages on scroll-up. The nomi surface backs both work conversations and the
  // companion's single session (which also absorbs every IM-channel turn and can
  // grow without bound), so a one-shot 10k fetch would crush the API/DOM.
  const historyPaging = useMessageLstCache(conversation_id, { windowed: true });
  usePendingConfirmationsRecovery(conversation_id);
  const updateLocalImage = LocalImageView.useUpdateLocalImage();
  useEffect(() => {
    updateLocalImage({ root: workspace });
  }, [workspace]);
  const conversationValue = useMemo<ConversationContextValue>(() => {
    return {
      conversation_id: conversation_id,
      workspace,
      type: 'nomi',
      cron_job_id,
      hideSendBox,
      loadedSkills,
      loadedMcpServers,
      loadedMcpStatuses,
    };
  }, [conversation_id, workspace, cron_job_id, hideSendBox, loadedSkills, loadedMcpServers, loadedMcpStatuses]);

  return (
    <ConversationProvider value={conversationValue}>
      <ConversationArtifactProvider conversation_id={conversation_id}>
        <div className='flex-1 flex flex-col px-20px min-h-0'>
          <FlexFullContainer>
            <MessageList
              className='flex-1'
              emptySlot={emptySlot}
              onLoadOlder={historyPaging.loadOlder}
              hasMoreOlder={historyPaging.hasMore}
              loadingOlder={historyPaging.loadingOlder}
            />
          </FlexFullContainer>
          <PinnedPlan />
          {!hideSendBox && (
            <NomiSendBox
              conversation_id={conversation_id}
              modelSelection={modelSelection}
              session_mode={session_mode}
              agent_name={agent_name}
              hideModeSelector={hideModeSelector}
            />
          )}
        </div>
      </ConversationArtifactProvider>
    </ConversationProvider>
  );
};

export default HOC.Wrapper(MessageListProvider, MessageListLoadingProvider, LocalImageView.Provider)(NomiChat);
