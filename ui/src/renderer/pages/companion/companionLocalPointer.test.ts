/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { isPointOverCompanionHitTarget } from './companionHitTarget';
import { parseCompanionLocalPointer, toCompanionClientPoint } from './companionLocalPointer';

const fakeHitTarget = (left: number, top: number, width: number, height: number) =>
  ({
    getBoundingClientRect: () => ({
      left,
      top,
      right: left + width,
      bottom: top + height,
      width,
      height,
    }),
  }) as HTMLElement;

describe('companion local pointer', () => {
  test('maps normalized native coordinates through the current DOM viewport', () => {
    const sample = parseCompanionLocalPointer({
      kind: 'point',
      backend: 'appkit',
      xRatio: 0.25,
      yRatio: 0.5,
    });

    expect(toCompanionClientPoint(sample, { width: 192, height: 171.2 })).toEqual({ x: 48, y: 85.6 });
    expect(toCompanionClientPoint(sample, { width: 240, height: 214 })).toEqual({ x: 60, y: 107 });
    expect(toCompanionClientPoint(sample, { width: 312, height: 278.2 })).toEqual({ x: 78, y: 139.1 });
  });

  test('preserves points outside the native window', () => {
    const sample = parseCompanionLocalPointer({
      kind: 'point',
      backend: 'x11',
      xRatio: -0.1,
      yRatio: 1.25,
    });

    expect(toCompanionClientPoint(sample, { width: 200, height: 100 })).toEqual({ x: -20, y: 125 });
  });

  test('accepts explicit Wayland fallback and rejects malformed responses', () => {
    expect(parseCompanionLocalPointer({ kind: 'unsupported', backend: 'wayland' })).toEqual({
      kind: 'unsupported',
      backend: 'wayland',
    });

    for (const value of [
      null,
      {},
      { kind: 'point', backend: 'appkit', xRatio: Number.NaN, yRatio: 0 },
      { kind: 'unsupported', backend: 'x11' },
    ]) {
      let rejected = false;
      try {
        parseCompanionLocalPointer(value);
      } catch {
        rejected = true;
      }
      expect(rejected).toBe(true);
    }
  });

  test('rejects invalid viewport geometry and unsupported samples', () => {
    const sample = parseCompanionLocalPointer({
      kind: 'point',
      backend: 'win32',
      xRatio: 0.5,
      yRatio: 0.5,
    });
    const unsupported = parseCompanionLocalPointer({ kind: 'unsupported', backend: 'other' });

    expect(toCompanionClientPoint(sample, { width: 0, height: 100 })).toBeNull();
    expect(toCompanionClientPoint(sample, { width: 100, height: Number.POSITIVE_INFINITY })).toBeNull();
    expect(toCompanionClientPoint(unsupported, { width: 100, height: 100 })).toBeNull();
  });

  test('feeds a mixed-DPI-independent local point into the existing DOM hit test', () => {
    const sample = parseCompanionLocalPointer({
      kind: 'point',
      backend: 'appkit',
      xRatio: 80 / 240,
      yRatio: 120 / 214,
    });
    const point = toCompanionClientPoint(sample, { width: 240, height: 214 });
    expect(point).not.toBeNull();

    expect(
      isPointOverCompanionHitTarget(point!.x, point!.y, [fakeHitTarget(50, 90, 80, 80)], {
        tolerancePx: 0,
        getStyle: () => ({ pointerEvents: 'auto' }),
      })
    ).toBe(true);
  });
});
