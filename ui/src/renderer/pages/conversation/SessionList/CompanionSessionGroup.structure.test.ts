/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

describe('CompanionSessionGroup structure', () => {
  test('uses sidebar overflow controls for long companion rosters', () => {
    const source = readFileSync(join(dirname(fileURLToPath(import.meta.url)), 'CompanionSessionGroup.tsx'), 'utf8');

    expect(source.includes('getVisibleCompanionEntries')).toBe(true);
    expect(source.includes('showAllCompanions')).toBe(true);
    expect(source.includes("t('sessionList.expandDisplay'")).toBe(true);
    expect(source.includes("t('sessionList.collapseDisplay')")).toBe(true);
  });
});
