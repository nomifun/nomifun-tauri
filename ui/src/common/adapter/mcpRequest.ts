/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMcpServer, IMcpServerTransport } from '@/common/config/storage';
import type { McpServerId } from '@/common/types/ids';

export interface McpConnectionTestRequest {
  mcp_server_id?: McpServerId;
  name: string;
  transport: IMcpServerTransport;
}

export const buildMcpConnectionTestRequest = (
  server: Pick<IMcpServer, 'mcp_server_id' | 'name' | 'transport'>
): McpConnectionTestRequest => ({
  mcp_server_id: server.mcp_server_id,
  name: server.name,
  transport: server.transport,
});
