/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import { getVisibleCompanionEntries } from './companionVisibleEntries';

const companions = (count: number): string[] => Array.from({ length: count }, (_, index) => `companion-${index + 1}`);

describe('getVisibleCompanionEntries', () => {
  test('keeps every companion visible when the roster fits within the default limit', () => {
    const visible = getVisibleCompanionEntries(companions(5), false);

    expect(visible.hasOverflow).toBe(false);
    expect(visible.hiddenCount).toBe(0);
    expect(visible.entries).toEqual(companions(5));
  });

  test('shows the first five companions by default when the roster overflows', () => {
    const visible = getVisibleCompanionEntries(companions(7), false);

    expect(visible.hasOverflow).toBe(true);
    expect(visible.hiddenCount).toBe(2);
    expect(visible.entries).toEqual(companions(5));
  });

  test('returns every companion after the user expands the overflow roster', () => {
    const visible = getVisibleCompanionEntries(companions(7), true);

    expect(visible.hasOverflow).toBe(true);
    expect(visible.hiddenCount).toBe(0);
    expect(visible.entries).toEqual(companions(7));
  });
});
