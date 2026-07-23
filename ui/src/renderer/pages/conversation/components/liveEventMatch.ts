/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Live session targets are already validated at their protocol/component
 * boundaries. Keep the entity kind in the comparison so equal UUID text from
 * different business domains never aliases.
 */
export const isLiveEventForTarget = (
  eventKind: 'conversation' | 'terminal',
  eventTargetId: import('@/common/types/ids').ConversationId | import('@/common/types/ids').TerminalId,
  kind: 'conversation' | 'terminal',
  id: import('@/common/types/ids').ConversationId | import('@/common/types/ids').TerminalId,
): boolean => eventKind === kind && eventTargetId === id;
