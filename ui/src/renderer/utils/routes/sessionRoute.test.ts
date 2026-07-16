/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import { parseSessionRoute } from './sessionRoute';

const CONVERSATION_ID = 'conv_0190f5fe-7c00-7a00-8000-000000000001';
const TERMINAL_ID = 'term_0190f5fe-7c00-7a00-8000-000000000002';

describe('parseSessionRoute', () => {
  test('returns a discriminated target for each canonical session route', () => {
    expect(parseSessionRoute(`/conversation/${CONVERSATION_ID}`)).toEqual({
      kind: 'conversation',
      id: CONVERSATION_ID,
    });
    expect(parseSessionRoute(`/terminal/${TERMINAL_ID}`)).toEqual({
      kind: 'terminal',
      id: TERMINAL_ID,
    });
  });

  test('never confuses terminal and conversation identifiers', () => {
    expect(parseSessionRoute(`/conversation/${TERMINAL_ID}`)).toBeNull();
    expect(parseSessionRoute(`/terminal/${CONVERSATION_ID}`)).toBeNull();
  });

  test('returns null instead of throwing for malformed or non-detail routes', () => {
    for (const pathname of [
      '/conversation/not-an-id',
      '/terminal/42',
      `/terminal/${TERMINAL_ID}/unexpected`,
      '/terminal-new',
      '/guid',
    ]) {
      expect(parseSessionRoute(pathname)).toBeNull();
    }
  });
});
