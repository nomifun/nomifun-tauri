/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import { parseConversationId, parseTerminalId, type ConversationId, type TerminalId } from '@/common/types/ids';
import type { WorkpathNode } from './workpathTree';
import {
  getBatchSelectionScopeState,
  getWorkpathBatchSelectionScope,
  toggleBatchSelectionScope,
} from './batchSelectionScopes';

const C1 = parseConversationId('0190f5fe-7c00-7a00-8000-000000000001');
const C2 = parseConversationId('0190f5fe-7c00-7a00-8000-000000000002');
const C3 = parseConversationId('0190f5fe-7c00-7a00-8000-000000000003');
const T1 = parseTerminalId('0190f5fe-7c00-7a00-8000-000000000001');
const T2 = parseTerminalId('0190f5fe-7c00-7a00-8000-000000000002');

const node = (
  key: string,
  interactive: ConversationId[],
  terminal: TerminalId[],
  displayName = key
): WorkpathNode => ({
  key,
  displayName,
  pinned: false,
  activityAt: 0,
  interactive: interactive.map((id, index) => ({
    kind: 'interactive',
    id,
    name: `c${index}`,
    pinned: false,
    pinnedAt: 0,
    activityAt: index,
    createdAt: index,
    conversation: { id } as never,
  })),
  terminal: terminal.map((id, index) => ({
    kind: 'terminal',
    id,
    name: `t${index}`,
    pinned: false,
    pinnedAt: 0,
    activityAt: index,
    createdAt: index,
    terminal: { id } as never,
  })),
});

describe('getWorkpathBatchSelectionScope', () => {
  test('returns every descendant row for a workpath aggregate node', () => {
    expect(getWorkpathBatchSelectionScope(node('/repo/app', [C2, C3], [T1], 'app'))).toEqual({
      conversationIds: [C2, C3],
      terminalIds: [T1],
    });
  });

  test('narrows a workpath aggregate node to one session kind', () => {
    const workpath = node('/repo/app', [C2, C3], [T1], 'app');

    expect(getWorkpathBatchSelectionScope(workpath, 'interactive')).toEqual({
      conversationIds: [C2, C3],
      terminalIds: [],
    });
    expect(getWorkpathBatchSelectionScope(workpath, 'terminal')).toEqual({
      conversationIds: [],
      terminalIds: [T1],
    });
  });
});

describe('toggleBatchSelectionScope', () => {
  test('adds a partially selected scope without dropping existing selections', () => {
    const next = toggleBatchSelectionScope(
      { conversationIds: [C2, C3], terminalIds: [T1] },
      { conversationIds: new Set([C1, C2]), terminalIds: new Set<TerminalId>() }
    );

    expect([...next.conversationIds].sort()).toEqual([C1, C2, C3].sort());
    expect([...next.terminalIds]).toEqual([T1]);
  });

  test('removes a fully selected scope and preserves other selections', () => {
    const next = toggleBatchSelectionScope(
      { conversationIds: [C2, C3], terminalIds: [T1] },
      { conversationIds: new Set([C1, C2, C3]), terminalIds: new Set([T1, T2]) }
    );

    expect([...next.conversationIds]).toEqual([C1]);
    expect([...next.terminalIds]).toEqual([T2]);
  });
});

describe('getBatchSelectionScopeState', () => {
  test('returns unchecked when no descendant rows are selected', () => {
    expect(
      getBatchSelectionScopeState(
        { conversationIds: [C1, C2], terminalIds: [T1] },
        { conversationIds: new Set(), terminalIds: new Set() }
      )
    ).toMatchObject({ checked: false, indeterminate: false, disabled: false });
  });

  test('returns checked when every descendant row is selected', () => {
    expect(
      getBatchSelectionScopeState(
        { conversationIds: [C1, C2], terminalIds: [T1] },
        { conversationIds: new Set([C1, C2]), terminalIds: new Set([T1]) }
      )
    ).toMatchObject({ checked: true, indeterminate: false, disabled: false });
  });

  test('returns indeterminate when only part of the aggregate node is selected', () => {
    expect(
      getBatchSelectionScopeState(
        { conversationIds: [C1, C2], terminalIds: [T1] },
        { conversationIds: new Set([C2]), terminalIds: new Set() }
      )
    ).toMatchObject({ checked: true, indeterminate: true, disabled: false });
  });

  test('returns disabled for aggregate nodes without selectable descendants', () => {
    expect(
      getBatchSelectionScopeState(
        { conversationIds: [], terminalIds: [] },
        { conversationIds: new Set([C1]), terminalIds: new Set([T1]) }
      )
    ).toMatchObject({ checked: false, indeterminate: false, disabled: true });
  });
});
