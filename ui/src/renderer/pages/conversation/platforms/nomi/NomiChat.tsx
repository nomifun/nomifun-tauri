/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import type { ConversationId, CronJobId } from '@/common/types/ids';

import type { IConversationMcpStatus } from '@/common/config/storage';
import type { ConversationContextValue } from '@/renderer/hooks/context/ConversationContext';
import { ConversationProvider } from '@/renderer/hooks/context/ConversationContext';
import FlexFullContainer from '@renderer/components/layout/FlexFullContainer';
import MessageList from '@renderer/pages/conversation/Messages/MessageList';
import { ConversationArtifactProvider } from '@renderer/pages/conversation/Messages/artifacts';
import {
  MessageListLoadingProvider,
  MessageListProvider,
  useMessageLstCache,
} from '@renderer/pages/conversation/Messages/hooks';
import { usePendingConfirmationsRecovery } from '@renderer/pages/conversation/Messages/usePendingConfirmationsRecovery';
import HOC from '@renderer/utils/ui/HOC';
import React, { useEffect, useMemo, useState } from 'react';
import LocalImageView from '@renderer/components/media/LocalImageView';
import NomiSendBox from './NomiSendBox';
import { mergeWithCapabilities, type AgentModeOption } from '@/renderer/utils/model/agentModes';
import { useNomiMessage } from './useNomiMessage';
import type { NomiModelSelection } from './useNomiModelSelection';

const NomiChat: React.FC<{
  conversation_id: ConversationId;
  workspace: string;
  modelSelection: NomiModelSelection;
  session_mode?: string;
  cron_job_id?: CronJobId;
  hideSendBox?: boolean;
  readOnly?: boolean;
  emptySlot?: React.ReactNode;
  loadedSkills?: string[];
  loadedMcpStatuses?: IConversationMcpStatus[];
  agent_name?: string;
  isProcessing?: boolean;
  /** Hide the permission/agent-mode selector in the send box (locked surfaces). */
  hideModeSelector?: boolean;
  /** Conversation collaborator-model control rendered after the main model. */
  collaboratorSelectorNode?: React.ReactNode;
  /** Extra right-side tools used by projected task transcripts. */
  extraRightTools?: React.ReactNode;
}> = ({
  conversation_id,
  workspace,
  modelSelection,
  session_mode,
  cron_job_id,
  hideSendBox,
  readOnly,
  emptySlot,
  loadedSkills,
  loadedMcpStatuses,
  agent_name,
  isProcessing,
  hideModeSelector,
  collaboratorSelectorNode,
  extraRightTools,
}) => {
  // Windowed history: load only the newest page on mount + lazily prepend older
  // pages on scroll-up. The nomi surface backs both work conversations and the
  // companion's single session (which also absorbs every IM-channel turn and can
  // grow without bound), so a one-shot 10k fetch would crush the API/DOM.
  const historyPaging = useMessageLstCache(conversation_id, { windowed: true });
  usePendingConfirmationsRecovery(conversation_id, { enabled: !readOnly });
  const [dynamicModes, setDynamicModes] = useState<AgentModeOption[]>([]);
  const turnActivity = useNomiMessage(conversation_id, {
    readOnly,
    onConfigChanged: (capabilities) => {
      const modes = (capabilities as { modes?: string[] })?.modes;
      if (modes && modes.length > 0) {
        setDynamicModes(mergeWithCapabilities('nomi', modes));
      }
    },
  });
  const updateLocalImage = LocalImageView.useUpdateLocalImage();
  useEffect(() => {
    updateLocalImage({ root: workspace });
  }, [workspace]);
  const resolvedIsProcessing = turnActivity.hasHydratedRunningState
    ? turnActivity.running
    : isProcessing === true || turnActivity.running;
  const conversationValue = useMemo<ConversationContextValue>(() => {
    return {
      conversation_id: conversation_id,
      workspace,
      type: 'nomi',
      cron_job_id,
      hideSendBox,
      readOnly,
      isProcessing: resolvedIsProcessing,
      loadedSkills,
      loadedMcpStatuses,
    };
  }, [
    conversation_id,
    workspace,
    cron_job_id,
    hideSendBox,
    readOnly,
    resolvedIsProcessing,
    loadedSkills,
    loadedMcpStatuses,
  ]);

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
          {!readOnly && !hideSendBox && (
            <NomiSendBox
              conversation_id={conversation_id}
              modelSelection={modelSelection}
              session_mode={session_mode}
              agent_name={agent_name}
              hideModeSelector={hideModeSelector}
              collaboratorSelectorNode={collaboratorSelectorNode}
              extraRightTools={extraRightTools}
              dynamicModes={dynamicModes}
              turnActivity={turnActivity}
            />
          )}
        </div>
      </ConversationArtifactProvider>
    </ConversationProvider>
  );
};

export default HOC.Wrapper(MessageListProvider, MessageListLoadingProvider, LocalImageView.Provider)(NomiChat);
