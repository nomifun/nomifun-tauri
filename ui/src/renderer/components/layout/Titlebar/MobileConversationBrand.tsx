/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import type { ConversationId } from '@/common/types/ids';

import { ipcBridge } from '@/common';
import { AgentLogoIcon } from '@/renderer/components/agent/AgentBadge';
import { usePresetInfo } from '@/renderer/hooks/agent/usePresetInfo';
import React from 'react';
import useSWR from 'swr';

type MobileConversationBrandProps = {
  conversation_id: ConversationId;
  fallbackTitle: string;
};

const MobileConversationBrand: React.FC<MobileConversationBrandProps> = ({ conversation_id, fallbackTitle }) => {
  const { data: conversation } = useSWR(
    conversation_id ? `mobile-titlebar.conversation.${conversation_id}` : null,
    () => ipcBridge.conversation.get.invoke({ conversation_id: conversation_id })
  );
  const { info: preset } = usePresetInfo(conversation || undefined);

  const backend =
    conversation?.type === 'acp'
      ? conversation.extra?.backend
      : conversation?.type === 'nomi'
        ? 'nomi'
        : conversation?.type === 'openclaw-gateway'
            ? 'openclaw-gateway'
            : conversation?.type === 'nanobot'
              ? 'nanobot'
              : conversation?.type === 'remote'
                ? 'remote'
                : undefined;

  const showLogo = Boolean(backend || preset);
  const title = conversation?.name || fallbackTitle;

  return (
    <span className='app-titlebar__brand-mobile'>
      {showLogo && (
        <AgentLogoIcon
          backend={backend}
          agent_name={title}
          agentLogo={preset?.logo}
          agentLogoIsEmoji={preset?.isEmoji}
        />
      )}
      <span className='app-titlebar__brand-text'>{title}</span>
    </span>
  );
};

export default MobileConversationBrand;
