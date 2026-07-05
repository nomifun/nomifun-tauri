/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = () => readFileSync(new URL('./LocalAgents.tsx', import.meta.url), 'utf8');

describe('LocalAgents detection refresh wiring', () => {
  test('keeps a manual re-scan control on the local agents surface', () => {
    const text = source();

    expect(text.includes('refreshCustomAgents')).toBe(true);
    expect(text.includes("data-testid='btn-refresh-local-agents'")).toBe(true);
    expect(text.includes('handleRefreshDetection')).toBe(true);
    expect(text.includes('settings.agentManagement.refreshDetection')).toBe(true);
  });
});
