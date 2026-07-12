/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { IMcpServer } from '@/common/config/storage';
import { buildMcpConnectionTestRequest } from './mcpRequest';

const transport: IMcpServer['transport'] = {
  type: 'sse',
  url: 'https://example.com/sse',
  headers: { Authorization: 'Bearer test' },
};

describe('buildMcpConnectionTestRequest', () => {
  test('keeps a persisted numeric id and sends only endpoint-owned fields', () => {
    const server: IMcpServer = {
      id: 1,
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
      id: 1,
      name: 'search',
      transport,
    });
  });

  test('omits the detected-server sentinel id', () => {
    expect(buildMcpConnectionTestRequest({ id: 0, name: 'detected', transport })).toEqual({
      name: 'detected',
      transport,
    });
  });
});
