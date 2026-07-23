/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseMessageId } from '@/common/types/ids';
import {
  isAcpEventForActiveTurn,
  isAcpThinkingBoundaryForTurn,
  normalizeAcpSlashCommands,
  shouldClearActiveRequestForStartedTurn,
  shouldProjectForeignAcpStreamEvent,
} from './useAcpMessage';

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

describe('ACP thinking turn correlation', () => {
  const currentTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000071');
  const oldTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000072');

  test('only lets a content boundary from the same explicit turn finish active thinking', () => {
    expect(isAcpThinkingBoundaryForTurn('text', currentTurnId, currentTurnId)).toBe(true);
    expect(isAcpThinkingBoundaryForTurn('text', currentTurnId, oldTurnId)).toBe(false);
    expect(isAcpThinkingBoundaryForTurn('thinking', currentTurnId, currentTurnId)).toBe(false);
  });

  test('uses stream order when a non-turn frame has no correlation id', () => {
    expect(isAcpThinkingBoundaryForTurn('text', currentTurnId, undefined)).toBe(true);
    expect(isAcpThinkingBoundaryForTurn('text', undefined, oldTurnId)).toBe(true);
  });

  test('does not let delayed foreign thinking replace the active turn tracker', () => {
    expect(isAcpEventForActiveTurn(currentTurnId, currentTurnId)).toBe(true);
    expect(isAcpEventForActiveTurn(oldTurnId, currentTurnId)).toBe(false);
    expect(isAcpEventForActiveTurn(undefined, currentTurnId)).toBe(true);
  });

  test('drops a stopped request correlation when a different authoritative turn starts', () => {
    expect(shouldClearActiveRequestForStartedTurn(currentTurnId, oldTurnId)).toBe(true);
    expect(shouldClearActiveRequestForStartedTurn(currentTurnId, currentTurnId)).toBe(false);
    expect(shouldClearActiveRequestForStartedTurn(null, currentTurnId)).toBe(false);
  });

  test('does not manufacture a standalone row from a foreign thinking completion', () => {
    expect(shouldProjectForeignAcpStreamEvent('thinking', { status: 'done', duration: 20 })).toBe(false);
    expect(shouldProjectForeignAcpStreamEvent('thinking', { status: 'thinking', content: '分析中' })).toBe(true);
    expect(shouldProjectForeignAcpStreamEvent('error', { content: 'old failure' })).toBe(true);
  });
});
