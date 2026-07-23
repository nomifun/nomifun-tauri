/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { parseConversationId, type ConversationId } from '@/common/types/ids';

export function parseScheduledConversationId(searchParams: URLSearchParams): ConversationId | null {
  if (searchParams.get('create') !== 'conversation') return null;

  const rawConversationId = searchParams.get('conversation_id');
  if (!rawConversationId) return null;

  try {
    return parseConversationId(rawConversationId);
  } catch {
    return null;
  }
}
