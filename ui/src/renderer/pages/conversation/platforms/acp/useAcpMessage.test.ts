/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { normalizeAcpSlashCommands } from './useAcpMessage';

describe('normalizeAcpSlashCommands', () => {
  test('filters commands without a usable name and stringifies structured descriptions', () => {
    expect(
      normalizeAcpSlashCommands([
        { name: 'fix', description: { scope: 'current file' } },
        { command: 'test', description: 'Run tests' },
        { name: { bad: true }, description: 'skip' },
      ])
    ).toEqual([
      {
        name: 'fix',
        description: '{\n  "scope": "current file"\n}',
        kind: 'template',
        source: 'acp',
        selectionBehavior: 'insert',
      },
      {
        name: 'test',
        description: 'Run tests',
        kind: 'template',
        source: 'acp',
        selectionBehavior: 'insert',
      },
    ]);
  });
});
