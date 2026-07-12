/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('companion suggestion pagination bridge', () => {
  test('exposes paged suggestion items and an offset parameter', () => {
    expect(source.includes('export interface ICompanionSuggestionPage')).toBe(true);
    expect(source.includes('items: ICompanionSuggestion[];')).toBe(true);
    expect(source.includes('total: number;')).toBe(true);
    expect(/listSuggestions: httpGet<\s*ICompanionSuggestionPage,/.test(source)).toBe(true);
    expect(source.includes('offset?: number')).toBe(true);
  });
});
