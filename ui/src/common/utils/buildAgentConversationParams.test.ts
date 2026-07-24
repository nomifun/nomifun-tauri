/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { buildAgentConversationParams } from './buildAgentConversationParams';
import type { TProviderWithModel } from '@/common/config/storage';
import { parseAgentId, parseProviderId, parseRemoteAgentId } from '@/common/types/ids';
import { parsePresetReference } from '@/common/types/agent/presetTypes';

const agentId = parseAgentId('0190f5fe-7c00-7a00-8000-000000000002');

const model: TProviderWithModel = {
  id: parseProviderId('0190f5fe-7c00-7a00-8000-000000000001'),
  name: 'Provider 1',
  platform: 'openai',
  base_url: 'https://example.invalid',
  api_key: '',
  use_model: 'model-1',
};

describe('buildAgentConversationParams preset contract', () => {
  test('sends only the preset reference at the top level for a preset launch', () => {
    const result = buildAgentConversationParams({
      backend: 'claude',
      name: 'Preset launch',
      agent_id: agentId,
      agent_name: 'Claude',
      preset_id: parsePresetReference('0190f5fe-7c00-7a00-8000-000000000001', 'user'),
      workspace: '/tmp/workspace',
      model,
      is_preset: true,
    });

    expect(result.preset_id).toBe('0190f5fe-7c00-7a00-8000-000000000001');
    expect('preset_id' in result.extra).toBe(false);
    expect('agent_id' in result.extra).toBe(false);
    expect('agent_name' in result.extra).toBe(false);
    expect('backend' in result.extra).toBe(false);
  });

  test('keeps bare Agent runtime identity and omits preset lineage', () => {
    const result = buildAgentConversationParams({
      backend: 'claude',
      name: 'Bare Agent launch',
      agent_id: agentId,
      agent_name: 'Claude',
      workspace: '/tmp/workspace',
      model,
    });

    expect(result.preset_id).toBeUndefined();
    expect(result.extra.agent_id).toBe(agentId);
    expect(result.extra.agent_name).toBe('Claude');
    expect(result.extra.backend).toBe('claude');
  });

  test('stores the selected remote-agent business id in snake_case', () => {
    const remoteAgentId = parseRemoteAgentId('0190f5fe-7c00-7a00-8000-000000000001');
    const result = buildAgentConversationParams({
      backend: 'remote',
      name: 'Remote OpenClaw',
      workspace: '/tmp/workspace',
      model,
      remote_agent_id: remoteAgentId,
    });

    expect(result.type).toBe('remote');
    expect(result.extra.remote_agent_id).toBe(remoteAgentId);
  });

  test('rejects a missing remote-agent business id', () => {
    let error: unknown;
    try {
      buildAgentConversationParams({
        backend: 'remote',
        name: 'Remote OpenClaw',
        workspace: '/tmp/workspace',
        model,
      });
    } catch (caught) {
      error = caught;
    }
    expect(error instanceof Error).toBe(true);
    expect((error as Error).message.includes('remote_agent_id')).toBe(true);
  });
});
