/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IMcpServer, IMcpServerTransport } from '@/common/config/storage';

export interface McpConnectionTestRequest {
  id?: number;
  name: string;
  transport: IMcpServerTransport;
}

export const buildMcpConnectionTestRequest = (
  server: Pick<IMcpServer, 'id' | 'name' | 'transport'>
): McpConnectionTestRequest => ({
  ...(server.id > 0 ? { id: server.id } : {}),
  name: server.name,
  transport: server.transport,
});
