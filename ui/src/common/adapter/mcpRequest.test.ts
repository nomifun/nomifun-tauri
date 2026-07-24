/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IMcpServer } from '@/common/config/storage';
import { parseMcpServerId } from '@/common/types/ids';
import { buildMcpConnectionTestRequest } from './mcpRequest';

const transport: IMcpServer['transport'] = {
  type: 'sse',
  url: 'https://example.com/sse',
  headers: { Authorization: 'Bearer test' },
};

describe('buildMcpConnectionTestRequest', () => {
  test('keeps a canonical persisted id and sends only endpoint-owned fields', () => {
    const id = parseMcpServerId('0190f5fe-7c00-7a00-8000-000000000001');
    const server: IMcpServer = {
      mcp_server_id: id,
      name: 'search',
      description: 'not part of test request',
      enabled: true,
      transport,
      tools: [{ name: 'search' }],
      last_test_status: 'connected',
      last_connected: 100,
      created_at: 10,
      updated_at: 20,
      original_json: '{}',
      builtin: false,
    };

    expect(buildMcpConnectionTestRequest(server)).toEqual({
      mcp_server_id: id,
      name: 'search',
      transport,
    });
    expect(Object.prototype.hasOwnProperty.call(buildMcpConnectionTestRequest(server), 'id')).toBe(
      false,
    );
  });
});
