/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** A canonical bare lowercase UUIDv7 business ID. */
const UUID_V7 =
  /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/**
 * Human-scannable short form of a stable entity locator, shared by list rows
 * and dropdowns that surface an ID or a path target inline.
 *
 * - UUIDv7 entity ids → show the final 12 UUID hex digits. UUIDv7's leading
 *   digits mostly encode time, so taking the leading characters is a poor
 *   discriminator for nearby creations. The complete id remains available via
 *   hover/copy affordances.
 * - Anything else (e.g. a workpath binding target) → the last path segment,
 *   capped so a long absolute path can't blow out the row.
 *
 * Replaces per-call-site truncation snippets while keeping non-ID path targets
 * readable.
 */
export const shortSessionId = (value: string | number): string => {
  const text = String(value);
  if (UUID_V7.test(text)) return text.slice(-12);
  const tail = text.split(/[\\/]/).filter(Boolean).pop() ?? text;
  return tail.length > 24 ? `…${tail.slice(-24)}` : tail;
};
