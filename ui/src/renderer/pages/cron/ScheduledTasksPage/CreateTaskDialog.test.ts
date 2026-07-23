/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./CreateTaskDialog.tsx', import.meta.url), 'utf8');

describe('CreateTaskDialog conversation id presentation', () => {
  test('formats and searches stable conversation UUIDs through shortSessionId without a # prefix', () => {
    expect(source.includes("import { shortSessionId } from '@renderer/utils/ui/shortId'")).toBe(true);
    expect(source.includes('const idLabel = shortSessionId(conv.id)')).toBe(true);
    expect(source.includes('const shortId = shortSessionId(conv.id).toLowerCase()')).toBe(true);
    expect(source.includes('`#${conv.id}`')).toBe(false);
  });
});
