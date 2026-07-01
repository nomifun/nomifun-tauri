/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

describe('CompanionConversation structure', () => {
  test('uses the route-level titlebar workspace toggle instead of self-contained panel toggles', () => {
    const source = readFileSync(new URL('./CompanionConversation.tsx', import.meta.url), 'utf8');

    expect(source.includes('selfContainedWorkspaceToggle')).toBe(false);
  });
});
