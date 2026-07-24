import { describe, expect, test } from 'bun:test';
import { parseMcpServerId } from '@/common/types/ids';
import { extensionMcpUiKey, mcpServerUiKey, parseExtensionMcpServers } from './extensionCatalog';

describe('parseExtensionMcpServers', () => {
  test('accepts only canonical source_key records without hiding valid siblings', () => {
    const result = parseExtensionMcpServers([
      {
        source_key: 'web-tools:browser',
        name: 'browser',
        description: 'Browser tools contributed by an extension',
        enabled: true,
        transport: {
          type: 'stdio',
          command: 'browser-mcp',
          args: ['--serve'],
        },
      },
      { source_key: 42, name: 'malformed contribution' },
      { source_key: 'constructor', name: 'missing extension namespace' },
      { source_key: 'web-tools:', name: 'empty local key' },
      { source_key: ':browser', name: 'empty extension name' },
      { source_key: 'web-tools:browser:extra', name: 'too many segments' },
      { source_key: 'WebTools:browser', name: 'uppercase extension name' },
      { source_key: 'web-tools:Browser', name: 'uppercase local key' },
      { source_key: 'web tools:browser', name: 'whitespace' },
      {
        source_key: 'web-tools:0190f5fe-7c00-7a00-8000-000000000003',
        name: 'bare UUIDv7 product identity',
      },
      {
        source_key: 'web-tools:mcp_0190f5fe-7c00-7a00-8000-000000000003',
        name: 'prefixed UUIDv7 product identity',
      },
      {
        source_key: 'search-tools:search',
        name: 'search',
        enabled: false,
        transport: {
          type: 'http',
          url: 'https://example.com/mcp',
        },
      },
      { source_key: 'plain-tools:no-description', name: 'no description', description: '   ' },
    ]);

    expect(result).toEqual([
      {
        source_key: 'web-tools:browser',
        name: 'browser',
        description: 'Browser tools contributed by an extension',
      },
      {
        source_key: 'search-tools:search',
        name: 'search',
      },
      {
        source_key: 'plain-tools:no-description',
        name: 'no description',
      },
    ]);
  });

  test('rejects generic id and old source-key aliases', () => {
    expect(
      parseExtensionMcpServers([
        { id: 'ext-web-tools-browser', name: 'legacy generic id' },
        { sourceKey: 'web-tools:browser', name: 'camelCase alias' },
        { contributionKey: 'web-tools:browser', name: 'old contribution alias' },
        { contribution_key: 'web-tools:browser', name: 'old snake-case alias' },
        {
          source_key: 'web-tools:browser',
          id: 'ext-web-tools-browser',
          name: 'mixed canonical and legacy',
        },
      ])
    ).toEqual([]);
  });

  test('keeps the first contribution for each duplicate source_key', () => {
    const result = parseExtensionMcpServers([
      { source_key: 'shared:tools', name: 'first', description: 'keep this one' },
      { source_key: 'shared:tools', name: 'second', description: 'ignore this one' },
      { source_key: 'safe:constructor', name: 'safe sibling' },
    ]);

    expect(result).toEqual([
      {
        source_key: 'shared:tools',
        name: 'first',
        description: 'keep this one',
      },
      {
        source_key: 'safe:constructor',
        name: 'safe sibling',
      },
    ]);
  });
});

describe('extensionMcpUiKey', () => {
  test('namespaces extension source keys away from shared UI state keys', () => {
    const contributions = parseExtensionMcpServers([
      { source_key: 'safe:constructor', name: 'constructor suffix' },
      { source_key: 'safe:__proto__', name: 'prototype suffix' },
    ]);

    expect(contributions.map((server) => extensionMcpUiKey(server.source_key))).toEqual([
      'extension:safe:constructor',
      'extension:safe:__proto__',
    ]);
  });

  test('namespaces repository IDs away from extension contribution keys', () => {
    expect(mcpServerUiKey(parseMcpServerId('0190f5fe-7c00-7a00-8000-000000000003'))).toBe('server:0190f5fe-7c00-7a00-8000-000000000003');
  });
});
