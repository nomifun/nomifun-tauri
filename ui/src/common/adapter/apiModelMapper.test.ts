/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { fromApiConversation } from './apiModelMapper';
import { parseMcpServerId, parseMessageId, parseRemoteAgentId } from '../types/ids';

// 最小 ApiConversation 片段：只构造 mapper 关心的字段
const apiConv = (o: Record<string, unknown>) => ({
  conversation_id: '0190f5fe-7c00-7a00-8000-000000000001',
  name: 'conv',
  type: 'acp',
  created_at: 1,
  modified_at: 2,
  ...o,
});

type MappedExtra =
  | {
      custom_workspace?: boolean;
      remote_agent_id?: ReturnType<typeof parseRemoteAgentId>;
    }
  | null
  | undefined;
const extraOf = (raw: Record<string, unknown>): MappedExtra => (fromApiConversation(raw) as { extra?: MappedExtra }).extra;

describe('fromApiConversation first-class fields', () => {
  test('maps the explicit wire conversation_id to the UI id and removes the wire field', () => {
    const mapped = fromApiConversation(apiConv({ extra: {} })) as {
      id?: string;
      conversation_id?: string;
    };
    expect(mapped.id).toBe('0190f5fe-7c00-7a00-8000-000000000001');
    expect(mapped.conversation_id).toBeUndefined();
  });

  test('rejects the removed generic wire id', () => {
    let error: unknown;
    try {
      fromApiConversation({
        id: '0190f5fe-7c00-7a00-8000-000000000001',
        name: 'legacy',
        type: 'acp',
        created_at: 1,
        modified_at: 2,
        extra: {},
      });
    } catch (caught) {
      error = caught;
    }
    expect(error instanceof Error).toBe(true);
  });

  test('keeps pin state at the conversation top level', () => {
    const mapped = fromApiConversation(
      apiConv({ pinned: true, pinned_at: 1712345678000, extra: {} }),
    ) as { pinned?: boolean; pinned_at?: number; extra?: Record<string, unknown> };
    expect(mapped.pinned).toBe(true);
    expect(mapped.pinned_at).toBe(1712345678000);
    expect(mapped.extra && 'pinned' in mapped.extra).toBe(false);
  });

  test('keeps the canonical remote-agent logical reference only', () => {
    const remoteAgentId = parseRemoteAgentId('0190f5fe-7c00-7a00-8000-000000000001');
    const extra = extraOf(apiConv({ type: 'remote', extra: { remote_agent_id: remoteAgentId } }));
    expect(extra?.remote_agent_id).toBe(remoteAgentId);
    expect(extra && 'remoteAgentId' in extra).toBe(false);
  });

  test('parses runtime active_turn_id as exact lifecycle authority', () => {
    const turnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000021');
    const mapped = fromApiConversation(
      apiConv({
        status: 'running',
        runtime: {
          state: 'running',
          is_processing: true,
          active_turn_id: turnId,
        },
        extra: {},
      })
    ) as { runtime?: { active_turn_id?: ReturnType<typeof parseMessageId> } };

    expect(mapped.runtime?.active_turn_id).toBe(turnId);
  });
});

describe('fromApiConversation 协作方案顶层契约', () => {
  test('保留顶层 execution_template_id，不从旧 extra 回填', () => {
    const currentTemplateId = '0190f5fe-7c00-7a00-8000-000000000001';
    const topLevel = fromApiConversation(
      apiConv({
        execution_template_id: currentTemplateId,
        extra: { execution_template_id: 'template-stale' },
      }),
    ) as { execution_template_id?: string };
    expect(topLevel.execution_template_id).toBe(currentTemplateId);

    const extraOnly = fromApiConversation(
      apiConv({ extra: { execution_template_id: 'template-stale' } }),
    ) as { execution_template_id?: string };
    expect(extraOnly.execution_template_id).toBeUndefined();
  });
});

describe('fromApiConversation MCP id boundaries', () => {
  test('keeps canonical UUIDv7 MCP identities across snapshots', () => {
    const mcpServerId = parseMcpServerId('0190f5fe-7c00-7a00-8000-000000000123');
    const mapped = fromApiConversation(
      apiConv({
        extra: {
          mcp_server_ids: [mcpServerId],
          mcp_statuses: [
            { mcp_server_id: mcpServerId, name: 'everything', status: 'loaded' },
          ],
          session_mcp_servers: [
            {
              mcp_server_id: mcpServerId,
              name: 'everything',
              transport: { type: 'stdio', command: 'npx' },
            },
          ],
        },
      }),
    ) as unknown as {
      extra: {
        mcp_server_ids: ReturnType<typeof parseMcpServerId>[];
        mcp_statuses: Array<{ mcp_server_id: ReturnType<typeof parseMcpServerId> }>;
        session_mcp_servers: Array<{ mcp_server_id: ReturnType<typeof parseMcpServerId> }>;
      };
    };

    expect(mapped.extra.mcp_server_ids).toEqual([mcpServerId]);
    expect(mapped.extra.mcp_statuses[0]?.mcp_server_id).toBe(mcpServerId);
    expect(mapped.extra.session_mcp_servers[0]?.mcp_server_id).toBe(mcpServerId);
  });

  test('rejects integer, numeric string, UUIDv4, uppercase, and prefixed MCP ids', () => {
    const invalidIds = [
      3,
      '3',
      '550e8400-e29b-41d4-a716-446655440000',
      '0190F5FE-7C00-7A00-8000-000000000123',
      'mcp_0190f5fe-7c00-7a00-8000-000000000123',
    ];
    for (const invalidId of invalidIds) {
      for (const extra of [
        { mcp_server_ids: [invalidId] },
        {
          mcp_statuses: [
            { mcp_server_id: invalidId, name: 'everything', status: 'loaded' },
          ],
        },
        {
          session_mcp_servers: [
            {
              mcp_server_id: invalidId,
              name: 'everything',
              transport: { type: 'stdio', command: 'npx' },
            },
          ],
        },
      ]) {
        let rejected = false;
        try {
          fromApiConversation(apiConv({ extra }));
        } catch {
          rejected = true;
        }
        expect(rejected).toBe(true);
      }
    }
  });

  test('rejects removed generic id fields for MCP status and session snapshots', () => {
    for (const extra of [
      { mcp_statuses: [{ id: 3, name: 'everything', status: 'loaded' }] },
      {
        session_mcp_servers: [
          {
            id: 3,
            name: 'everything',
            transport: { type: 'stdio', command: 'npx' },
          },
        ],
      },
    ]) {
      let rejected = false;
      try {
        fromApiConversation(apiConv({ extra }));
      } catch {
        rejected = true;
      }
      expect(rejected).toBe(true);
    }
  });
});
