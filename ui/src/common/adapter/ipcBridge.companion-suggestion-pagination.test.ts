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
    expect(source.includes('listSuggestions: withResponseMap(')).toBe(true);
    expect(/listSuggestions: withResponseMap\(\s*httpGet<\{ items: unknown\[\]; total: number \}/.test(source)).toBe(true);
    expect(source.includes('(raw): ICompanionSuggestionPage')).toBe(true);
    expect(source.includes('raw.items.map(fromApiCompanionSuggestion)')).toBe(true);
    expect(source.includes('offset?: number')).toBe(true);
  });

  test('uses suggestion_id for suggestion wire identity and decision calls', () => {
    expect(source.includes('suggestion_id: CompanionSuggestionId;')).toBe(true);
    expect(source.includes('suggestion_id: parseCompanionSuggestionId(value.suggestion_id)')).toBe(true);
    expect(source.includes('{ suggestion_id: CompanionSuggestionId; accept: boolean }')).toBe(true);
    expect(source.includes('/api/companion/suggestions/${p.suggestion_id}/decide')).toBe(true);
    expect(/^\s*id:\s*CompanionSuggestionId;/m.test(source)).toBe(false);
    expect(source.includes('parseCompanionSuggestionId(value.id)')).toBe(false);
  });
});
