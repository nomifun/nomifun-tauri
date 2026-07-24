import type { IResponseMessage } from '@/common/adapter/ipcBridge';
import type { TChatConversation } from '@/common/config/storage';

export type ConversationRuntimeAuthority = 'idle' | 'processing' | 'unknown';

export const getConversationRuntimeAuthority = (
  conversation?: Pick<TChatConversation, 'runtime' | 'status'> | null
): ConversationRuntimeAuthority => {
  if (!conversation) return 'idle';

  // The durable aggregate terminal status dominates stale process projections:
  // a Finished conversation can never be raised back to Running by UI state.
  if (conversation.status === 'finished') return 'idle';

  if (conversation.status === 'running') {
    return conversation.runtime?.is_processing === true &&
      conversation.runtime.active_turn_id != null
      ? 'processing'
      : 'unknown';
  }

  if (conversation.status === 'pending') {
    return conversation.runtime?.is_processing === true ||
      conversation.runtime?.active_turn_id != null
      ? 'unknown'
      : 'idle';
  }

  // Legacy/malformed snapshots are never authority to start or settle a turn.
  return 'unknown';
};

export const isConversationProcessing = (conversation?: Pick<TChatConversation, 'runtime' | 'status'> | null) => {
  return getConversationRuntimeAuthority(conversation) === 'processing';
};

/** A complete projection is delivered over `message.stream` for realtime
 * rendering, but it does not own a model turn and intentionally has no later
 * `finish` / `turn.completed` event. */
export const isCompleteMessageProjection = (
  message?: Pick<IResponseMessage, 'stream_complete'> | null
): boolean => message?.stream_complete === true;
