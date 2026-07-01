/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export const COMPANION_COLLAPSED_LIST_LIMIT = 5;

export type VisibleCompanionEntries<T> = {
  entries: T[];
  hasOverflow: boolean;
  hiddenCount: number;
};

export function getVisibleCompanionEntries<T>(
  entries: T[],
  expanded: boolean,
  limit = COMPANION_COLLAPSED_LIST_LIMIT
): VisibleCompanionEntries<T> {
  const hasOverflow = entries.length > limit;

  if (expanded || !hasOverflow) {
    return {
      entries,
      hasOverflow,
      hiddenCount: 0,
    };
  }

  const visibleEntries = entries.slice(0, limit);

  return {
    entries: visibleEntries,
    hasOverflow,
    hiddenCount: entries.length - visibleEntries.length,
  };
}
