/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./TerminalSessionPage.tsx', import.meta.url), 'utf8');

describe('TerminalSessionPage workspace rail collapse wiring', () => {
  test('keeps terminal file auto-expand scoped to the current terminal session', () => {
    expect(source.includes('autoExpandOnFiles: true')).toBe(true);
    expect(source.includes('workspaceEventKey: String(session.id)')).toBe(true);
  });
});
