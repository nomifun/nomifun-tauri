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

/**
 * OrchestratorNodeBinding — supplied by {@link ProjectedWorkerView} for a DAG worker
 * node so the reused composer can double as the node's 启动前配置 台: picking a model in
 * the composer's own selector is written THROUGH as this node's per-node override (in
 * addition to the live conversation model). The parent owns the merge with the node's
 * preset, so this stays a single callback — the view never touches the persistence
 * layer for node config.
 */
export type OrchestratorNodeBinding = {
  /** Write the composer's model pick through as this node's per-node model override.
   * THROWS on failure — the caller treats it best-effort (the live model switch has
   * already succeeded). */
  applyModelOverride: (providerId: string, model: string) => Promise<void>;
};

/** Nomi sub-component manages model selection state without adding a ChatLayout wrapper */
const NomiReadOnlyChat: React.FC<{
  conversation: NomiConversation;
  agent_name?: string;
  hideSendBox?: boolean;
  nodeBinding?: OrchestratorNodeBinding;
  extraRightTools?: React.ReactNode;
}> = ({ conversation, agent_name, hideSendBox, nodeBinding, extraRightTools }) => {
  const onSelectModel = useCallback(
    async (_provider: IProvider, modelName: string) => {
      const selected = { ..._provider, use_model: modelName } as TProviderWithModel;
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) {
        void saveNomiDefaultModel(_provider.id, modelName);
        // Write the model pick THROUGH as this node's override — one pick pins both the
        // live chat model and the node's next-重跑 model. Best-effort: a failure here
        // must not break the live model switch (which already succeeded above).
        if (nodeBinding) {
          try {
            await nodeBinding.applyModelOverride(_provider.id, modelName);
          } catch (e) {
            console.error('[NomiReadOnlyChat] write-through node override failed:', e);
          }
        }
      }
      return Boolean(ok);
    },
    [conversation.id, nodeBinding]
  );

  const modelSelection = useNomiModelSelection({ initialModel: conversation.model, onSelectModel });

  return (
    <NomiChat
      conversation_id={conversation.id}
      workspace={conversation.extra.workspace}
      modelSelection={modelSelection}
      agent_name={agent_name}
      hideSendBox={hideSendBox}
      extraRightTools={extraRightTools}
    />
  );
};

type ReadOnlyConversationViewProps = {
  conversation: TChatConversation;
  hideSendBox?: boolean;
  agent_name?: string;
  /** When set, the reused nomi composer also drives this DAG node's config
   * (model write-through). Only the `nomi` branch consumes it. */
  nodeBinding?: OrchestratorNodeBinding;
  /** Extra right-tools node injected into the nomi composer (e.g. a 预置要求 pill). */
  extraRightTools?: React.ReactNode;
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
const ReadOnlyConversationView: React.FC<ReadOnlyConversationViewProps> = ({
  conversation,
  hideSendBox,
  agent_name,
  nodeBinding,
  extraRightTools,
}) => {
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
            nodeBinding={nodeBinding}
            extraRightTools={extraRightTools}
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
