/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TMessage } from '@/common/chat/chatLib';

const SYNTHETIC_INCOMPLETE_PREFIX = 'The turn ended before this tool completed:';

export const isSupersededPlanToolFailure = (message: TMessage, laterMessages: TMessage[]): boolean => {
  if (message.type !== 'tool_call') return false;
  if (message.content.name !== 'update_plan' || message.content.status !== 'error') return false;
  if (typeof message.content.output !== 'string' || !message.content.output.startsWith(SYNTHETIC_INCOMPLETE_PREFIX)) {
    return false;
  }

  const failedAt = message.created_at ?? 0;
  return laterMessages.some(
    (candidate) =>
      candidate.type === 'plan' &&
      candidate.content.session_id === 'update_plan' &&
      (candidate.created_at ?? failedAt) >= failedAt
  );
};
