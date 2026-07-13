/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const pageSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
const controlCss = readFileSync(new URL('../../styles/theme-control-contract.css', import.meta.url), 'utf8');

describe('open capabilities checkbox selection treatment', () => {
  test('uses enhanced checkbox and dark-mode text treatments throughout the capability page', () => {
    expect(pageSource.includes("className='open-capabilities-page'")).toBe(true);
    expect(pageSource.includes("className='open-capabilities-domain-checkbox mt-2px shrink-0'")).toBe(true);
    expect(pageSource.includes('open-capabilities-domain-badge')).toBe(true);
    expect(pageSource.includes('open-capabilities-domain-description')).toBe(true);
    expect(pageSource.includes("open-capabilities-domain-card${checked ? ' is-selected' : ''}")).toBe(true);
    expect(controlCss.includes('.open-capabilities-domain-checkbox .arco-checkbox-mask')).toBe(true);
    expect(controlCss.includes('.open-capabilities-domain-checkbox.arco-checkbox-checked .arco-checkbox-mask')).toBe(true);
    expect(controlCss.includes('--enhanced-checkbox-selected-bg')).toBe(true);
    expect(controlCss.includes('transition: transform 160ms ease-out')).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .open-capabilities-domain-card")).toBe(true);
    expect(controlCss.includes('background-color: transparent !important')).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .open-capabilities-domain-card.is-selected")).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .open-capabilities-domain-description")).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .open-capabilities-domain-badge")).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .open-capabilities-page .text-t-tertiary")).toBe(true);
    expect(controlCss.includes("body[arco-theme='dark'] .open-capabilities-page .text-t-secondary")).toBe(true);
    expect(
      /body\[arco-theme='dark'\] \.open-capabilities-domain-card \{\n  background-color: transparent !important;\n  border: 1px solid/.test(
        controlCss
      )
    ).toBe(true);
    expect(
      /body\[arco-theme='dark'\] \.open-capabilities-domain-card\.is-selected \{\n  background-color: color-mix[\s\S]*?\n  border: 1px solid/.test(
        controlCss
      )
    ).toBe(true);
  });
});
