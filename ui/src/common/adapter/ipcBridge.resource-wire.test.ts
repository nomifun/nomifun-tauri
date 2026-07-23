/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const bridgeSource = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');
const managedModelSource = readFileSync(
  new URL('../types/provider/managedModelService.ts', import.meta.url),
  'utf8'
);

describe('named resource wire IDs', () => {
  test('does not expose generic id parameters for core resource locators', () => {
    for (const expected of [
      '{ conversation_id: ConversationId }',
      '{ terminal_id: TerminalId }',
      '{ provider_id: ProviderId }',
      '{ knowledge_base_id: KnowledgeBaseId }',
      '{ credential_id: ConnectorCredentialId }',
    ]) {
      expect(bridgeSource.includes(expected)).toBe(true);
    }
    expect(managedModelSource.includes('model_id: string;')).toBe(true);
    expect(
      managedModelSource.includes('export interface SetManagedModelEnabledRequest {\n  id:')
    ).toBe(false);
    expect(
      managedModelSource.includes('export interface CheckManagedModelHealthRequest {\n  id:')
    ).toBe(false);

    for (const legacy of [
      '/api/conversations/${p.id}',
      '/api/terminals/${p.id}',
      '/api/providers/${p.id}',
      '/api/knowledge/bases/${p.id}',
      '/api/knowledge/connectors/credentials/${p.credentialId}',
      '/api/model-services/free/models/${encodeURIComponent(p.id)}',
    ]) {
      expect(bridgeSource.includes(legacy)).toBe(false);
    }
    expect(
      bridgeSource.includes(
        'connector credential wire payload must use credentialId, not generic id'
      )
    ).toBe(true);
    expect(bridgeSource.includes('credentialId: parseConnectorCredentialId')).toBe(true);
    expect(bridgeSource.includes('parseConnectorCredentialId(base.source.credentialRef)')).toBe(true);
  });
});
