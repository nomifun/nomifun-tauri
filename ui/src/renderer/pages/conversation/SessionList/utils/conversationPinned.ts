/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TChatConversation } from '@/common/config/storage';

export const isConversationPinned = (conversation: TChatConversation): boolean => {
  return conversation.pinned === true;
};
