/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

describe('application sider overflow handling', () => {
  test('scrolls the navigation body while keeping the settings group pinned', () => {
    expect(source.includes("'flex-1 min-h-0 overflow-y-auto overflow-x-hidden'")).toBe(true);
    expect(source.includes("'shrink-0 mt-auto pt-8px flex flex-col gap-2px")).toBe(true);
  });
});
