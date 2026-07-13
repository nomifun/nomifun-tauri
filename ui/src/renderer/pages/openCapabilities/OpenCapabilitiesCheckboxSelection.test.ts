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
  test('uses the enhanced theme-aware checkbox treatment for every capability domain card', () => {
    expect(pageSource.includes("className='open-capabilities-domain-checkbox mt-2px shrink-0'")).toBe(true);
    expect(controlCss.includes('.open-capabilities-domain-checkbox .arco-checkbox-mask')).toBe(true);
    expect(controlCss.includes('.open-capabilities-domain-checkbox.arco-checkbox-checked .arco-checkbox-mask')).toBe(true);
    expect(controlCss.includes('--enhanced-checkbox-selected-bg')).toBe(true);
    expect(controlCss.includes('transition: transform 160ms ease-out')).toBe(true);
  });
});
