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

  test('keeps the workspace tool rail at the far right of the expanded panel', () => {
    const panelIndex = source.indexOf("className='!bg-1 relative layout-sider'");
    const railIndex = source.indexOf('<WorkspaceToolRail');

    expect(panelIndex >= 0).toBe(true);
    expect(railIndex >= 0).toBe(true);
    expect(panelIndex < railIndex).toBe(true);
  });
});
