/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { DEFAULT_THEME_ID, PRESET_THEMES } from './presets';

describe('display theme presets', () => {
  test('uses the quiet Codex neutral preset as the first default theme', () => {
    const ids = PRESET_THEMES.map((theme) => theme.id);
    const codexIndex = ids.indexOf('codex-neutral');

    expect(DEFAULT_THEME_ID).toBe('codex-neutral');
    expect(codexIndex).toBe(0);
    expect(codexIndex).toBeLessThan(ids.indexOf('rhythm-dark'));
    expect(codexIndex).toBeLessThan(ids.indexOf('neon-rainbow'));
    expect(PRESET_THEMES[codexIndex]?.name).toBe('经典');
  });
});
