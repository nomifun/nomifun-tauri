import { describe, expect, test } from 'bun:test';
import type { IMcpServer } from '@/common/config/storage';
import { parseMcpServerId } from '@/common/types/ids';
import { toSessionMcpServer } from './catalog';

const transport: IMcpServer['transport'] = {
  type: 'stdio',
  command: 'npx',
  args: ['-y', '@modelcontextprotocol/server-everything'],
};

describe('toSessionMcpServer', () => {
  test('preserves a canonical catalog id in the session snapshot', () => {
    const id = parseMcpServerId('0190f5fe-7c00-7a00-8000-000000000003');
    expect(toSessionMcpServer({ mcp_server_id: id, name: 'everything', transport })).toEqual({
      mcp_server_id: id,
      name: 'everything',
      transport,
    });
  });
});
