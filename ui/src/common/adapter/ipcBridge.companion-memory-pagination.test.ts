/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('companion memory pagination bridge', () => {
  test('exposes paged memory items with a filtered total', () => {
    expect(source.includes('export interface ICompanionMemoryPage')).toBe(true);
    expect(source.includes('items: ICompanionMemory[];')).toBe(true);
    expect(source.includes('total: number;')).toBe(true);
    expect(/listMemories: httpGet<\s*ICompanionMemoryPage,/.test(source)).toBe(true);
  });
});
