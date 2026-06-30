/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import { describe, expect, test } from 'bun:test';
import { bumpCtrlC, createCtrlCState, isCtrlC } from './ctrlCEscalation';

describe('isCtrlC', () => {
  test('true only for the ETX byte', () => {
    expect(isCtrlC('\x03')).toBe(true);
    expect(isCtrlC('c')).toBe(false);
    expect(isCtrlC('\r')).toBe(false);
    expect(isCtrlC('\x03\x03')).toBe(false); // a paste, not a single press
  });
});

describe('bumpCtrlC', () => {
  test('escalates on the threshold-th press within the window', () => {
    let s = createCtrlCState();
    let r = bumpCtrlC(s, 1000, 1500, 3);
    expect(r.escalate).toBe(false);
    s = r.state;
    r = bumpCtrlC(s, 1200, 1500, 3);
    expect(r.escalate).toBe(false);
    s = r.state;
    r = bumpCtrlC(s, 1400, 1500, 3);
    expect(r.escalate).toBe(true);
  });

  test('drops presses older than the window so slow taps never escalate', () => {
    let s = createCtrlCState();
    let r = bumpCtrlC(s, 0, 1500, 3);
    s = r.state;
    r = bumpCtrlC(s, 2000, 1500, 3); // first hit aged out
    s = r.state;
    r = bumpCtrlC(s, 4000, 1500, 3); // second aged out
    expect(r.escalate).toBe(false);
    expect(r.state.hits.length).toBe(1);
  });

  test('clears the window after escalation so the next burst starts fresh', () => {
    let s = createCtrlCState();
    bumpCtrlC(s, 100, 1500, 2);
    const r = bumpCtrlC({ hits: [100] }, 200, 1500, 2);
    expect(r.escalate).toBe(true);
    expect(r.state.hits.length).toBe(0);
  });
});
