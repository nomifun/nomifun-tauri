/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 * Based on AionUi (https://github.com/iOfficeAI/AionUi)
 */

import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import type { TChatConversation } from '@/common/config/storage';
import { mutate } from 'swr';

export async function getConversationOrNull(conversation_id: number): Promise<TChatConversation | null> {
  try {
    return await ipcBridge.conversation.get.invoke({ id: conversation_id });
  } catch (error) {
    if (isBackendHttpError(error) && error.status === 404 && error.code === 'NOT_FOUND') {
      return null;
    }
    throw error;
  }
}

export async function refreshConversationCache(conversation_id: number): Promise<void> {
  const conversation = await getConversationOrNull(conversation_id);
  if (!conversation) return;

  await mutate<TChatConversation>(`conversation/${conversation_id}`, conversation, false);
}

/**
 * Seed the SWR cache for a conversation we already hold in full — e.g. the
 * object the `conversation.create` endpoint just returned. This lets the
 * conversation content page (`useSWR('conversation/${id}')` in
 * `pages/conversation/index.tsx`) resolve synchronously from cache and render
 * immediately, skipping the redundant `conversation.get` round-trip that would
 * otherwise gate the whole content tree behind a blank spinner.
 *
 * `create` and `get` both map through `fromApiConversation`, so the shapes are
 * identical and this seed is safe. `revalidate = false` avoids an immediate
 * refetch; SWR's default mount revalidation still refreshes in the background
 * without blocking the first paint.
 */
export function seedConversationCache(conversation: TChatConversation): void {
  void mutate<TChatConversation>(`conversation/${conversation.id}`, conversation, false);
}
