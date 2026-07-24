/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const bridgeSource = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');
const typeSource = readFileSync(
  new URL('../../renderer/utils/model/agentTypes.ts', import.meta.url),
  'utf8'
);

describe('agent metadata wire ID contract', () => {
  test('uses agent_id without a generic id compatibility path', () => {
    expect(typeSource.includes('agent_id: AgentId;')).toBe(true);
    expect(typeSource.includes('\n  id: string;')).toBe(false);
    expect(bridgeSource.includes('AgentMetadata legacy field "id" is not accepted')).toBe(true);
    expect(bridgeSource.includes('agent_id: parseAgentId(value.agent_id)')).toBe(true);
    expect(bridgeSource.includes("value.agent_source === 'custom' || value.agent_source === 'extension'")).toBe(false);
    expect(bridgeSource.includes('/api/agents/custom/${p.agent_id}')).toBe(true);
    expect(bridgeSource.includes('/api/agents/${p.agent_id}/enabled')).toBe(true);
    expect(bridgeSource.includes('/api/agents/custom/${p.id}')).toBe(false);
    expect(bridgeSource.includes('/api/agents/${p.id}/enabled')).toBe(false);
  });
});
