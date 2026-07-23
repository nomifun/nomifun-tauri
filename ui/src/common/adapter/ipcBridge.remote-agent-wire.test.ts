/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const bridgeSource = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');
const typeSource = readFileSync(new URL('../types/agent/remoteAgentTypes.ts', import.meta.url), 'utf8');

describe('remote-agent wire ID contract', () => {
  test('uses remote_agent_id throughout the adapter and response type', () => {
    expect(typeSource.includes('remote_agent_id: RemoteAgentId;')).toBe(true);
    expect(typeSource.includes('\n  id: RemoteAgentId;')).toBe(false);
    expect(bridgeSource.includes('remote_agent_id: parseRemoteAgentId(value.remote_agent_id)')).toBe(true);
    expect(bridgeSource.includes('{ remote_agent_id: RemoteAgentId }')).toBe(true);
    expect(bridgeSource.includes('/api/remote-agents/${p.remote_agent_id}')).toBe(true);
    expect(bridgeSource.includes('{ id: RemoteAgentId }')).toBe(false);
    expect(bridgeSource.includes('/api/remote-agents/${p.id}')).toBe(false);
  });
});
