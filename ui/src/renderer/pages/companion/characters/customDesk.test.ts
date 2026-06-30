/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, it } from 'vitest';
import { customDeskSpec, FIGURE_HEIGHTS, MAX_WINDOW_WIDTH, MIN_WINDOW_WIDTH, SIZE_MIN, SIZE_MAX } from './customDesk';

describe('customDeskSpec', () => {
  it('computes window from aspect and tier (no sizePx override)', () => {
    // m → figure 210; a slightly-landscape aspect keeps width between MIN and MAX (no clamp).
    const d = customDeskSpec({ aspect: 1.2, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'm' });
    expect(d.figureHeight).toBe(210);
    expect(d.windowHeight).toBe(274); // figure 210 + CHROME_HEIGHT 64
    expect(d.windowWidth).toBe(Math.ceil(210 * 1.2) + 28); // 280, within [MIN, MAX]
  });

  it('uses sizePx as the figure height when set, overriding the tier', () => {
    // sizePx 320 wins over tier 'm' (210). aspect 0.9 → width within [MIN, MAX].
    const d = customDeskSpec({ aspect: 0.9, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'm', sizePx: 320 });
    expect(d.figureHeight).toBe(320);
    expect(d.windowHeight).toBe(384); // 320 + 64
    expect(d.windowWidth).toBe(Math.ceil(320 * 0.9) + 28); // 316
  });

  it('clamps sizePx to [SIZE_MIN, SIZE_MAX]', () => {
    const big = customDeskSpec({ aspect: 0.5, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'm', sizePx: 1000 });
    expect(big.figureHeight).toBe(SIZE_MAX); // 400
    const small = customDeskSpec({ aspect: 1, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'l', sizePx: 50 });
    expect(small.figureHeight).toBe(SIZE_MIN); // 140 (full-body, above BUST_MAX_SIZE 130)
  });

  it('ignores a degenerate sizePx and falls back to the tier', () => {
    const nan = customDeskSpec({ aspect: 1, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'l', sizePx: Number.NaN });
    expect(nan.figureHeight).toBe(FIGURE_HEIGHTS.l); // 280
    const zero = customDeskSpec({ aspect: 1, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 's', sizePx: 0 });
    expect(zero.figureHeight).toBe(FIGURE_HEIGHTS.s); // 150
  });

  it('clamps extreme wide images to MAX_WINDOW_WIDTH and shrinks the figure to fit', () => {
    // sizePx 400 at aspect 2.0 → raw width ceil(800)+28 = 828 > 400 → clamp.
    const d = customDeskSpec({ aspect: 2.0, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'l', sizePx: 400 });
    expect(d.windowWidth).toBe(MAX_WINDOW_WIDTH); // 400
    expect(d.figureHeight).toBe(Math.floor((MAX_WINDOW_WIDTH - 28) / 2.0)); // 186
  });

  it('never narrower than the classic window (skinny images keep chat usable)', () => {
    const d = customDeskSpec({ aspect: 0.3, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 's' });
    // ceil(150*0.3)+28 = 73 → clamped up; figure keeps its tier height
    expect(d.windowWidth).toBe(MIN_WINDOW_WIDTH);
    expect(d.figureHeight).toBe(150);
  });

  it('survives degenerate aspect values', () => {
    const d = customDeskSpec({ aspect: Number.NaN, headBox: { x: 0.3, y: 0, w: 0.3, h: 0.3 }, sizeTier: 'm' });
    expect(Number.isFinite(d.windowWidth)).toBe(true);
    expect(d.figureHeight).toBe(210);
  });

  it('size tiers map to fixed heights and slider bounds are sane', () => {
    expect(FIGURE_HEIGHTS).toEqual({ s: 150, m: 210, l: 280 });
    expect(SIZE_MIN).toBe(140);
    expect(SIZE_MAX).toBe(400);
    expect(MAX_WINDOW_WIDTH).toBe(400);
  });
});
