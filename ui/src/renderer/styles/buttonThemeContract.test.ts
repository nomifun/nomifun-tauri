/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { PRESET_THEMES } from '@renderer/pages/settings/DisplaySettings/presets';

const arcoOverrides = readFileSync(new URL('./arco-override.css', import.meta.url), 'utf8');

describe('button theme contract', () => {
  test('primary disabled buttons use dedicated readable tokens instead of a brand fill', () => {
    expect(arcoOverrides.includes('.arco-btn-primary.arco-btn-disabled')).toBe(true);
    expect(arcoOverrides.includes('background-color: var(--button-primary-disabled-bg')).toBe(true);
    expect(arcoOverrides.includes('border-color: var(--button-primary-disabled-border')).toBe(true);
    expect(arcoOverrides.includes('color: var(--button-primary-disabled-text')).toBe(true);
    expect(arcoOverrides.includes('background-color: var(--aou-2) !important')).toBe(false);
  });

  test('every built-in theme declares primary button foreground and disabled state tokens in both modes', () => {
    for (const theme of PRESET_THEMES) {
      const css = theme.css || '';

      expect(css.match(/--button-primary-text:/g)?.length).toBe(2);
      expect(css.match(/--button-primary-disabled-bg:/g)?.length).toBe(2);
      expect(css.match(/--button-primary-disabled-border:/g)?.length).toBe(2);
      expect(css.match(/--button-primary-disabled-text:/g)?.length).toBe(2);
    }
  });

  test('theme signature primary button rules do not style disabled buttons as active CTAs', () => {
    for (const theme of PRESET_THEMES) {
      const css = theme.css || '';
      if (!css.includes('.arco-btn-primary')) continue;
      const primarySectionStart = css.indexOf('.arco-btn-primary');
      const primarySectionEnd = css.indexOf('/*', primarySectionStart + 1);
      const primarySection = css.slice(primarySectionStart, primarySectionEnd === -1 ? css.length : primarySectionEnd);

      expect(css.includes('.arco-btn-primary:not(.arco-btn-disabled):not(.arco-btn-status-danger)')).toBe(true);
      expect(css.includes('.arco-btn-primary:not(.arco-btn-status-danger)')).toBe(false);
      expect(primarySection.includes('color: #ffffff;')).toBe(false);
      expect(primarySection.includes('color: #171717;')).toBe(false);
      expect(primarySection.includes('color: var(--button-primary-text);')).toBe(true);
    }
  });
});
