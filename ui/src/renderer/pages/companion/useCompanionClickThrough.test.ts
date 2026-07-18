/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./useCompanionClickThrough.ts', import.meta.url), 'utf8');
const desktopCapability = readFileSync(
  new URL('../../../../../apps/desktop/capabilities/default.json', import.meta.url),
  'utf8'
);

describe('companion click-through wiring', () => {
  test('uses native local samples and never rebuilds them from global geometry', () => {
    expect(source.includes('getCompanionLocalPointer')).toBe(true);
    expect(source.includes('CompanionClickThroughController')).toBe(true);
    expect(source.includes('cursorPosition')).toBe(false);
    expect(source.includes('outerPosition')).toBe(false);
    expect(source.includes('devicePixelRatio')).toBe(false);
    expect(source.includes('onMoved')).toBe(false);
    expect(source.includes('onResized')).toBe(false);
  });

  test('keeps normal and recovery scheduling explicit', () => {
    expect(source.includes('RECOVERY_INTERVAL_MS = 1000')).toBe(true);
    expect(source.includes("mode === 'poll' ? intervalMs : RECOVERY_INTERVAL_MS")).toBe(true);
    expect(source.includes("addEventListener('pointermove'")).toBe(true);
    expect(source.includes("addEventListener('pointerleave'")).toBe(true);
  });

  test('passes live interaction state and no longer requests global cursor access', () => {
    expect(source.includes('controller.tick(() =>')).toBe(true);
    expect(desktopCapability.includes('core:window:allow-cursor-position')).toBe(false);
  });
});
