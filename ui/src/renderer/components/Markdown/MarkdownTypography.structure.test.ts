/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const markdownSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
const shadowSource = readFileSync(new URL('./ShadowView.tsx', import.meta.url), 'utf8');

describe('Markdown typography controls', () => {
  test('lets message surfaces override the Shadow DOM body typography', () => {
    expect(markdownSource.includes('fontSize?: string')).toBe(true);
    expect(markdownSource.includes('lineHeight?: string')).toBe(true);
    expect(markdownSource.includes('<ShadowView fontSize={fontSize} lineHeight={lineHeight}>')).toBe(true);
    expect(shadowSource.includes('fontSize?: string')).toBe(true);
    expect(shadowSource.includes('lineHeight?: string')).toBe(true);
    expect(shadowSource.includes("const resolvedFontSize = fontSize ?? (isMobile ? '14px' : '16px');")).toBe(true);
    expect(shadowSource.includes("const resolvedLineHeight = lineHeight ?? (isMobile ? '19.6px' : '28px');")).toBe(true);
    expect(shadowSource.includes('const usesExplicitTypography = Boolean(fontSize || lineHeight);')).toBe(true);
    expect(shadowSource.includes("margin-block-start: ${usesExplicitTypography ? '10px' : '16px'};")).toBe(true);
    expect(shadowSource.includes("font-size: ${usesExplicitTypography ? resolvedFontSize : '24px'};")).toBe(true);
    expect(shadowSource.includes("font-size: ${usesExplicitTypography ? resolvedFontSize : '16px'};")).toBe(true);
  });
});
