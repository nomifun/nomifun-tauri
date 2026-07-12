/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TChatConversation } from '@/common/config/storage';
import { useArcoMessage } from '@/renderer/utils/ui/useArcoMessage';
import React from 'react';
import ChatWorkspace from '../Workspace';
import type { WorkspaceExtraTab } from '../Workspace/types';

const ChatSlider: React.FC<{
  conversation?: TChatConversation;
  extraTabs?: WorkspaceExtraTab[];
}> = ({ conversation, extraTabs }) => {
  const [messageApi, messageContext] = useArcoMessage({ maxCount: 1 });

  let workspaceNode: React.ReactNode = null;
  if (conversation?.type === 'acp' && conversation.extra?.workspace) {
    workspaceNode = (
      <ChatWorkspace
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        isTemporaryWorkspace={
          (conversation.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
        }
        eventPrefix='acp'
        messageApi={messageApi}
      ></ChatWorkspace>
    );
  } else if (conversation?.type === 'codex' && conversation.extra?.workspace) {
    workspaceNode = (
      <ChatWorkspace
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        isTemporaryWorkspace={
          (conversation.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
        }
        eventPrefix='codex'
        messageApi={messageApi}
      ></ChatWorkspace>
    );
  } else if (conversation?.type === 'nomi' && conversation.extra?.workspace) {
    workspaceNode = (
      <ChatWorkspace
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        isTemporaryWorkspace={
          (conversation.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
        }
        eventPrefix='nomi'
        messageApi={messageApi}
        extraTabs={extraTabs}
      ></ChatWorkspace>
    );
  } else if (conversation?.type === 'openclaw-gateway' && conversation.extra?.workspace) {
    workspaceNode = (
      <ChatWorkspace
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        isTemporaryWorkspace={
          (conversation.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
        }
        eventPrefix='openclaw-gateway'
        messageApi={messageApi}
      ></ChatWorkspace>
    );
  } else if (conversation?.type === 'nanobot' && conversation.extra?.workspace) {
    workspaceNode = (
      <ChatWorkspace
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        isTemporaryWorkspace={
          (conversation.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
        }
        eventPrefix='nanobot'
        messageApi={messageApi}
      ></ChatWorkspace>
    );
  } else if (conversation?.type === 'remote' && conversation.extra?.workspace) {
    workspaceNode = (
      <ChatWorkspace
        conversation_id={conversation.id}
        workspace={conversation.extra.workspace}
        isTemporaryWorkspace={
          (conversation.extra as { is_temporary_workspace?: boolean } | undefined)?.is_temporary_workspace
        }
        eventPrefix='remote'
        messageApi={messageApi}
      ></ChatWorkspace>
    );
  }

  if (!workspaceNode) {
    return <div></div>;
  }

  return (
    <>
      {messageContext}
      {workspaceNode}
    </>
  );
};

export default ChatSlider;
