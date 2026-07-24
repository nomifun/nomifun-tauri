/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import { parseConversationId, parseTerminalId, type ConversationId, type TerminalId } from '@/common/types/ids';
import { getVisibleWorkpathEntries } from './workpathVisibleEntries';
import type { WorkpathNode } from './workpathTree';

const conversationIds = [
  '0190f5fe-7c00-7a00-8000-000000000001',
  '0190f5fe-7c00-7a00-8000-000000000002',
  '0190f5fe-7c00-7a00-8000-000000000003',
  '0190f5fe-7c00-7a00-8000-000000000004',
  '0190f5fe-7c00-7a00-8000-000000000005',
  '0190f5fe-7c00-7a00-8000-000000000006',
].map(parseConversationId);

const terminalIds = [
  '0190f5fe-7c00-7a00-8000-000000000010',
  '0190f5fe-7c00-7a00-8000-000000000011',
  '0190f5fe-7c00-7a00-8000-000000000012',
  '0190f5fe-7c00-7a00-8000-000000000013',
  '0190f5fe-7c00-7a00-8000-000000000014',
  '0190f5fe-7c00-7a00-8000-000000000015',
  '0190f5fe-7c00-7a00-8000-000000000016',
].map(parseTerminalId);

const node = (interactive: ConversationId[], terminal: TerminalId[]): WorkpathNode => ({
  key: '/repo/app',
  displayName: 'app',
  pinned: false,
  activityAt: 0,
  interactive: interactive.map((id, index) => ({
    kind: 'interactive',
    id,
    name: `interactive-${index}`,
    pinned: false,
    pinnedAt: 0,
    activityAt: index,
    createdAt: index,
    conversation: { id } as never,
  })),
  terminal: terminal.map((id, index) => ({
    kind: 'terminal',
    id,
    name: `terminal-${index}`,
    pinned: false,
    pinnedAt: 0,
    activityAt: index,
    createdAt: index,
    terminal: { id } as never,
  })),
});

describe('getVisibleWorkpathEntries', () => {
  test('keeps all entries visible when each session kind is within the collapsed limit', () => {
    const visible = getVisibleWorkpathEntries(node(conversationIds.slice(0, 4), terminalIds.slice(0, 2)), false);

    expect(visible.hasOverflow).toBe(false);
    expect(visible.hiddenCount).toBe(0);
    expect(visible.interactive.map((item) => item.id)).toEqual(conversationIds.slice(0, 4));
    expect(visible.terminal.map((item) => item.id)).toEqual(terminalIds.slice(0, 2));
  });

  test('collapses each session kind independently while collapsed', () => {
    const visible = getVisibleWorkpathEntries(node(conversationIds, terminalIds), false);

    expect(visible.hasOverflow).toBe(true);
    expect(visible.hiddenCount).toBe(3);
    expect(visible.interactive.map((item) => item.id)).toEqual(conversationIds.slice(0, 5));
    expect(visible.terminal.map((item) => item.id)).toEqual(terminalIds.slice(0, 5));
  });

  test('returns every child session when expanded', () => {
    const visible = getVisibleWorkpathEntries(node(conversationIds.slice(0, 4), terminalIds.slice(0, 3)), true);

    expect(visible.hiddenCount).toBe(0);
    expect(visible.interactive.map((item) => item.id)).toEqual(conversationIds.slice(0, 4));
    expect(visible.terminal.map((item) => item.id)).toEqual(terminalIds.slice(0, 3));
  });
});
