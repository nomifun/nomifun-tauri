/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Whether a live `*.statusChanged` WS event targets THIS per-session control.
 *
 * The event payload's `target_id` arrives as a STRING at runtime (the backend
 * models conversation/terminal ids as a `String` and serialises them as a JSON
 * string, e.g. `"37"`), while a control's `id` prop is a NUMBER (the INTEGER
 * session id). A strict `event.target_id === id` therefore NEVER matches at
 * runtime (`"37" === 37` is `false`), so the conversation-header AutoWork / IDMM
 * controls silently stopped updating from live events and only reflected their
 * mount/GET state — leaving the header out of sync with the live session-list
 * icon (the header showed armed/orange while the sidebar correctly turned
 * intervening/green). Coerce both sides to string so the match is type-agnostic.
 */
export const isLiveEventForTarget = (
  eventKind: string,
  eventTargetId: string | number,
  kind: string,
  id: string | number
): boolean => eventKind === kind && String(eventTargetId) === String(id);
