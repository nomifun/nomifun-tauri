/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import {
  tryParseEntityId,
  type ConversationId,
  type SessionTarget,
  type TerminalId,
} from '@/common/types/ids';

/**
 * Parses the two canonical session detail routes without throwing.
 *
 * Persistent layout components (titlebar, session sidebar and shortcuts) stay
 * mounted while the leaf route changes. They must never use a strict entity-id
 * parser during render: a route for the other session kind, or a malformed URL,
 * would otherwise take down the entire shared application shell.
 */
export const parseSessionRoute = (pathname: string): SessionTarget | null => {
  const match = pathname.match(/^\/(conversation|terminal)\/([^/?#]+)\/?$/);
  if (!match) return null;

  if (match[1] === 'conversation') {
    const id: ConversationId | null = tryParseEntityId('conversation', match[2]);
    return id ? { kind: 'conversation', id } : null;
  }

  const id: TerminalId | null = tryParseEntityId('terminal', match[2]);
  return id ? { kind: 'terminal', id } : null;
};
