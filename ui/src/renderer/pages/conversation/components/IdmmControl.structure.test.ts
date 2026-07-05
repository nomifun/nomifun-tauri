/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('IdmmControl structure', () => {
  test('shows visible expand/collapse text on configuration headers', () => {
    const source = readSource(new URL('./IdmmControl.tsx', import.meta.url));

    expect(source.includes("t(open ? 'idmm.collapseConfig' : 'idmm.expandConfig')")).toBe(true);
    expect(source.includes("aria-label={t(open ? 'idmm.collapseConfig' : 'idmm.expandConfig')}")).toBe(true);
    expect(source.includes("aria-label={t(strategyOpen ? 'idmm.collapseConfig' : 'idmm.expandConfig')}")).toBe(true);
  });

  test('locks watch configuration whenever the watch is enabled, including draft mode', () => {
    const source = readSource(new URL('./IdmmControl.tsx', import.meta.url));

    expect(source.includes('const faultLocked = cfg.fault_watch.enabled;')).toBe(true);
    expect(source.includes('const decisionLocked = cfg.decision_watch.enabled;')).toBe(true);
    expect(source.includes('watchEnabled && !isDraft')).toBe(false);
  });
});
