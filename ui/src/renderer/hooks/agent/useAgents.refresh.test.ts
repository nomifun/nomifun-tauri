/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = () => readFileSync(new URL('./useAgents.ts', import.meta.url), 'utf8');

describe('useAgents detection refresh wiring', () => {
  test('auto-refreshes shared detected-agent cache without every consumer issuing its own POST', () => {
    const text = source();

    expect(text.includes('AGENT_AUTO_REFRESH_MIN_INTERVAL_MS')).toBe(true);
    expect(text.includes('agentAutoRefreshPromise')).toBe(true);
    expect(text.includes('refreshDetectedAgentsIfStale')).toBe(true);
    expect(text.includes('useEffect')).toBe(true);
    expect(text.includes('void refreshDetectedAgentsIfStale()')).toBe(true);
  });

  test('refresh hits the backend refresh endpoint and then updates the shared SWR key', () => {
    const text = source();

    expect(text.includes('ipcBridge.acpConversation.refreshCustomAgents.invoke()')).toBe(true);
    expect(text.includes('mutate(DETECTED_AGENTS_SWR_KEY)')).toBe(true);
  });
});
