/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('public agent wire ID contract', () => {
  test('uses public_agent_id and audit_entry_id without generic id aliases', () => {
    expect(source.includes('public_agent_id: PublicAgentId;')).toBe(true);
    expect(source.includes('audit_entry_id: PublicAgentAuditEntryId;')).toBe(true);
    expect(
      source.includes(
        'public_agent_id: parsePublicAgentId(agent.public_agent_id)'
      )
    ).toBe(true);
    expect(
      source.includes(
        'audit_entry_id: parsePublicAgentAuditEntryId(entry.audit_entry_id)'
      )
    ).toBe(true);
    expect(
      source.includes(
        'public agent wire payload must use public_agent_id, not id'
      )
    ).toBe(true);
    expect(
      source.includes(
        'public agent audit wire payload must use audit_entry_id, not id'
      )
    ).toBe(true);
    expect(source.includes('/api/public-agents/${p.public_agent_id}')).toBe(
      true
    );
    expect(source.includes('id: parsePublicAgentId(agent.id)')).toBe(false);
    expect(
      source.includes('id: parsePublicAgentAuditEntryId(entry.id)')
    ).toBe(false);
    expect(source.includes('{ id: PublicAgentId')).toBe(false);
  });
});
