/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IProvider, TChatConversation } from '@/common/config/storage';
import { Spin } from '@arco-design/web-react';
import React, { Suspense, useCallback } from 'react';
import { useNomiModelSelection } from '@/renderer/pages/conversation/platforms/nomi/useNomiModelSelection';
import { PreviewProvider } from '@/renderer/pages/conversation/Preview';
import { browserStorageKey } from '@/common/utils/browserStorageKey';

const AcpChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/acp/AcpChat'));
const NomiChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/nomi/NomiChat'));
const OpenClawChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/openclaw/OpenClawChat'));
const NanobotChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/nanobot/NanobotChat'));
const RemoteChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/remote/RemoteChat'));

// Narrow to Nomi conversations so model field is always available
type NomiConversation = Extract<TChatConversation, { type: 'nomi' }>;

/** Nomi sub-component supplies locked model state without adding a ChatLayout wrapper. */
const NomiReadOnlyChat: React.FC<{
  conversation: NomiConversation;
  agent_name?: string;
}> = ({ conversation, agent_name }) => {
  const lockedSelect = useCallback(async (_provider: IProvider, _modelName: string) => false, []);

  const modelSelection = useNomiModelSelection({
    initialModel: conversation.model,
    onSelectModel: lockedSelect,
  });

  return (
    <NomiChat
      conversation_id={conversation.id}
      workspace={conversation.extra.workspace}
      modelSelection={modelSelection}
      agent_name={agent_name}
      hideSendBox
      readOnly
    />
  );
};

export type ReadOnlyConversationViewProps = {
  conversation: TChatConversation;
  agent_name?: string;
};

/**
 * Routes to the correct platform chat component based on conversation type and
 * renders it read-only (send box hidden). Used by the collaboration view to
 * mirror a participant's live conversation record.
 *
 * Does NOT wrap in ChatLayout — the parent supplies its own chrome. It DOES,
 * however, mount its OWN {@link PreviewProvider}: the platform chat's
 * `MessageList` (via `useAutoPreviewOfficeFiles`) calls `usePreviewContext()`,
 * which throws when no provider is in scope. The collaboration view renders this
 * inside an Arco `Drawer` without a `ChatLayout`, so without this self-contained
 * provider clicking a task crashed the window. The namespace includes the
 * immutable attempt identity when available (falling back to the conversation
 * id), so projected transcripts never restore another attempt's tabs.
 * `subscribeGlobalOpen={false}` also prevents this viewer from stealing
 * agent-driven global preview opens.
 */
const ReadOnlyConversationView: React.FC<ReadOnlyConversationViewProps> = ({ conversation, agent_name }) => {
  const transcriptStorageKey =
    conversation.execution_attempt_id != null
      ? browserStorageKey('workspace-preview', 'execution-attempt', conversation.execution_attempt_id)
      : conversation.execution_step_id != null
        ? browserStorageKey('workspace-preview', 'execution-step', conversation.execution_step_id)
        : browserStorageKey('workspace-preview', 'conversation', conversation.id);
  const content = (() => {
    switch (conversation.type) {
      case 'acp':
        return (
          <AcpChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace}
            backend={conversation.extra?.backend || 'claude'}
            initialModelId={(conversation.extra as { current_model_id?: string } | undefined)?.current_model_id}
            session_mode={conversation.extra?.session_mode}
            agent_name={agent_name ?? (conversation.extra as { agent_name?: string })?.agent_name}
            hideSendBox
            readOnly
          />
        );
      case 'nomi':
        return (
          <NomiReadOnlyChat
            key={conversation.id}
            conversation={conversation as NomiConversation}
            agent_name={agent_name}
          />
        );
      case 'openclaw-gateway':
        return (
          <OpenClawChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            hideSendBox
            readOnly
          />
        );
      case 'nanobot':
        return (
          <NanobotChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            hideSendBox
            readOnly
          />
        );
      case 'remote':
        return (
          <RemoteChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            hideSendBox
            readOnly
          />
        );
      default:
        return null;
    }
  })();

  return (
    <PreviewProvider
      key={transcriptStorageKey}
      persistNamespace={transcriptStorageKey}
      subscribeGlobalOpen={false}
    >
      <Suspense fallback={<Spin loading className='flex flex-1 items-center justify-center' />}>{content}</Suspense>
    </PreviewProvider>
  );
};

export default ReadOnlyConversationView;
