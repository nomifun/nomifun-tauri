/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { createContext, useCallback, useContext, useMemo, useState } from 'react';

/**
 * Snapshot of the message the user just submitted from the Guid composer, used
 * to render an immediate conversation-shaped loading state while the backend
 * mints the conversation row.
 */
export type PendingConversation = {
  /** The first message the user typed (echoed as a right-aligned bubble). */
  input: string;
  /** Attachment paths, if any — shown as a small count under the bubble. */
  files?: string[];
  /**
   * Whether this entry will send `input` as the conversation's first turn.
   * AutoWork entries start a backend loop WITHOUT sending a first message, so
   * the loading caption differs ("正在启动 AutoWork…" vs "正在创建会话…").
   */
  sendsInitialMessage: boolean;
};

type PendingConversationContextValue = {
  pending: PendingConversation | null;
  /** Show the loading overlay immediately (called synchronously on send). */
  begin: (payload: PendingConversation) => void;
  /** Tear the overlay down, deferred one frame to hide the mount seam. */
  end: () => void;
};

// Stable no-op fallback for consumers rendered outside the provider (e.g. any
// non-shell route). They still get callable handlers so they never null-check.
const NOOP_VALUE: PendingConversationContextValue = {
  pending: null,
  begin: () => undefined,
  end: () => undefined,
};

// After the real conversation page is navigated to, keep the overlay up for a
// brief beat so the destination has time to mount and NomiSendBox can echo the
// same user bubble in the same place — then the swap has nothing to flicker.
// Worst case (slower echo) it degrades to the destination's own shimmer
// skeleton, which is the same visual family, so no jarring blank flash either
// way. Purely cosmetic; not a correctness wait.
const OVERLAY_TEARDOWN_DELAY_MS = 280;

const PendingConversationContext = createContext<PendingConversationContextValue | null>(null);

/**
 * Hosts the "creating conversation" transition state. Provided at the
 * {@link ConversationShell} level (which wraps the shared `<Outlet/>` and
 * persists across `/guid` ↔ `/conversation/:id`), so the overlay begun on the
 * Guid page stays mounted continuously through the navigation into the real
 * conversation — no fake id, no route juggling.
 */
export const PendingConversationProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [pending, setPending] = useState<PendingConversation | null>(null);

  const begin = useCallback((payload: PendingConversation) => {
    setPending(payload);
  }, []);

  const end = useCallback(() => {
    // Defer teardown so the freshly-navigated conversation page has committed
    // and echoed the first message before we uncover it (see the constant note).
    if (typeof window !== 'undefined' && typeof window.setTimeout === 'function') {
      window.setTimeout(() => setPending(null), OVERLAY_TEARDOWN_DELAY_MS);
    } else {
      setPending(null);
    }
  }, []);

  const value = useMemo<PendingConversationContextValue>(() => ({ pending, begin, end }), [pending, begin, end]);

  return <PendingConversationContext.Provider value={value}>{children}</PendingConversationContext.Provider>;
};

export const usePendingConversation = (): PendingConversationContextValue => {
  return useContext(PendingConversationContext) ?? NOOP_VALUE;
};
