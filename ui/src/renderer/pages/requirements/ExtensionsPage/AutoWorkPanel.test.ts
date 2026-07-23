/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./AutoWorkPanel.tsx', import.meta.url), 'utf8');

describe('AutoWorkPanel stable target ids', () => {
  test('uses compact UUID labels without legacy numeric #N semantics', () => {
    expect(source.includes('shortSessionId(binding.target_id)')).toBe(true);
    expect(source.includes('title={String(binding.target_id)}')).toBe(true);
    expect(source.includes('`#${binding.target_id}`')).toBe(false);
    expect(source.includes('INTEGER primary key')).toBe(false);
  });
});
