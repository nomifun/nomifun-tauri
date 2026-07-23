/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React from 'react';

import BindTargetOptionRow from '@renderer/components/base/BindTargetOptionRow';
import type { TChatConversation } from '@/common/config/storage';
import { shortSessionId } from '@renderer/utils/ui/shortId';

/**
 * Renders a two-line option node for the cron "specified conversation" Select:
 * - Line 1: conversation name (or compact UUID fallback) + a dimmed backend/type badge.
 * - Line 2: the conversation workspace path (middle-truncated) followed by the
 *   compact UUID suffix. The full stable UUID remains available on hover.
 */
export const renderConversationOption = (conv: TChatConversation): React.ReactNode => {
  const idLabel = shortSessionId(conv.id);
  const extra = conv.extra as unknown as { workspace?: string; backend?: string } | undefined;
  return (
    <div className='min-w-0' title={conv.id}>
      <BindTargetOptionRow
        title={conv.name || idLabel}
        badge={extra?.backend || conv.type}
        path={extra?.workspace}
        idLabel={idLabel}
      />
    </div>
  );
};
