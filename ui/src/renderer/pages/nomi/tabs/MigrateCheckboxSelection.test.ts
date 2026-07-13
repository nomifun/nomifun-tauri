/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const migrateTabSource = readFileSync(new URL('./MigrateTab.tsx', import.meta.url), 'utf8');
const controlCss = readFileSync(new URL('../../../styles/theme-control-contract.css', import.meta.url), 'utf8');
const classicThemeCss = readFileSync(
  new URL('../../settings/DisplaySettings/presets/codex-neutral.css', import.meta.url),
  'utf8'
);

describe('migration event checkbox selection treatment', () => {
  test('uses a contained selected fill, centered checkmark, and motion-safe transition', () => {
    expect(migrateTabSource.includes("className='migrate-events-checkbox'")).toBe(true);
    expect(controlCss.includes('.migrate-events-checkbox .arco-checkbox-mask')).toBe(true);
    expect(controlCss.includes('.migrate-events-checkbox.arco-checkbox-checked .arco-checkbox-mask')).toBe(true);
    expect(controlCss.includes('background-color: var(--enhanced-checkbox-selected-bg, var(--control-selected-bg, var(--color-primary)))')).toBe(
      true
    );
    expect(controlCss.includes('border: 1px solid var(--control-idle-border, var(--color-border-3)) !important')).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .migrate-events-checkbox")).toBe(false);
    expect(controlCss.includes('background-color: #151515')).toBe(false);
    expect(classicThemeCss.includes('--enhanced-checkbox-selected-bg: #000000')).toBe(true);
    expect(controlCss.includes('.migrate-events-checkbox .arco-checkbox-mask-icon')).toBe(true);
    expect(controlCss.includes('width: 8px')).toBe(true);
    expect(controlCss.includes('inset: 0')).toBe(true);
    expect(controlCss.includes('transition: transform 160ms ease-out')).toBe(true);
    expect(controlCss.includes('@media (prefers-reduced-motion: reduce)')).toBe(true);
  });
});
