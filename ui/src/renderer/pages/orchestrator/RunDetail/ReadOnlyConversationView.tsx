/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { IProvider, TChatConversation, TProviderWithModel } from '@/common/config/storage';
import { Spin } from '@arco-design/web-react';
import React, { Suspense, useCallback } from 'react';
import { useNomiModelSelection } from '@/renderer/pages/conversation/platforms/nomi/useNomiModelSelection';
import { saveNomiDefaultModel } from '@/renderer/pages/guid/hooks/agentSelectionUtils';
import { PreviewProvider } from '@/renderer/pages/conversation/Preview';

const AcpChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/acp/AcpChat'));
const NomiChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/nomi/NomiChat'));
const OpenClawChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/openclaw/OpenClawChat'));
const NanobotChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/nanobot/NanobotChat'));
const RemoteChat = React.lazy(() => import('@/renderer/pages/conversation/platforms/remote/RemoteChat'));

// Narrow to Nomi conversations so model field is always available
type NomiConversation = Extract<TChatConversation, { type: 'nomi' }>;

/** Nomi sub-component manages model selection state without adding a ChatLayout wrapper */
const NomiReadOnlyChat: React.FC<{
  conversation: NomiConversation;
  agent_name?: string;
  hideSendBox?: boolean;
}> = ({ conversation, agent_name, hideSendBox }) => {
  const onSelectModel = useCallback(
    async (_provider: IProvider, modelName: string) => {
      const selected = { ..._provider, use_model: modelName } as TProviderWithModel;
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) void saveNomiDefaultModel(_provider.id, modelName);
      return Boolean(ok);
    },
    [conversation.id]
  );

  const modelSelection = useNomiModelSelection({ initialModel: conversation.model, onSelectModel });

  return (
    <NomiChat
      conversation_id={conversation.id}
      workspace={conversation.extra.workspace}
      modelSelection={modelSelection}
      agent_name={agent_name}
      hideSendBox={hideSendBox}
    />
  );
};

type ReadOnlyConversationViewProps = {
  conversation: TChatConversation;
  hideSendBox?: boolean;
  agent_name?: string;
};

/**
 * Routes to the correct platform chat component based on conversation type and
 * renders it read-only (send box hidden). Used by the orchestrator's worker
 * transcript drawer to mirror a worker's live conversation record.
 *
 * Does NOT wrap in ChatLayout — the parent supplies its own chrome. It DOES,
 * however, mount its OWN {@link PreviewProvider}: the platform chat's
 * `MessageList` (via `useAutoPreviewOfficeFiles`) calls `usePreviewContext()`,
 * which throws when no provider is in scope. The orchestrator renders this view
 * inside an Arco `Drawer` without a `ChatLayout`, so without this self-contained
 * provider clicking a DAG node crashed the window. We use a dedicated namespace
 * and `subscribeGlobalOpen={false}` so this read-only viewer never persists into
 * the main conversation's preview bucket nor steals agent-driven global preview
 * opens (mirrors the terminal surface's per-surface provider convention).
 */
const ReadOnlyConversationView: React.FC<ReadOnlyConversationViewProps> = ({ conversation, hideSendBox, agent_name }) => {
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
            hideSendBox={hideSendBox}
          />
        );
      case 'codex': // Legacy: codex now uses ACP protocol
        return (
          <AcpChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace}
            backend='codex'
            initialModelId={(conversation.extra as { current_model_id?: string } | undefined)?.current_model_id}
            agent_name={agent_name ?? (conversation.extra as { agent_name?: string })?.agent_name}
            hideSendBox={hideSendBox}
          />
        );
      case 'nomi':
        return (
          <NomiReadOnlyChat
            key={conversation.id}
            conversation={conversation as NomiConversation}
            agent_name={agent_name}
            hideSendBox={hideSendBox}
          />
        );
      case 'openclaw-gateway':
        return (
          <OpenClawChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            hideSendBox={hideSendBox}
          />
        );
      case 'nanobot':
        return (
          <NanobotChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            hideSendBox={hideSendBox}
          />
        );
      case 'remote':
        return (
          <RemoteChat
            key={conversation.id}
            conversation_id={conversation.id}
            workspace={conversation.extra?.workspace ?? ''}
            hideSendBox={hideSendBox}
          />
        );
      default:
        return null;
    }
  })();

  return (
    <PreviewProvider persistNamespace='orchestrator-transcript' subscribeGlobalOpen={false}>
      <Suspense fallback={<Spin loading className='flex flex-1 items-center justify-center' />}>{content}</Suspense>
    </PreviewProvider>
  );
};

export default ReadOnlyConversationView;
