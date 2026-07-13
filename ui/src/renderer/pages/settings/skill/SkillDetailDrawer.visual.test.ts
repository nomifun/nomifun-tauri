/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./SkillDetailDrawer.tsx', import.meta.url), 'utf8');

describe('SkillDetailDrawer metadata layout', () => {
  test('keeps the location row free of a container divider', () => {
    expect(source.includes("className='flex min-w-0 items-start gap-10px pt-10px'")).toBe(true);
    expect(source.includes('border-t border-solid border-[var(--color-border-1)]')).toBe(false);
    expect(source.includes("className='flex min-w-0 flex-1 items-center gap-6px'")).toBe(true);
    expect(source.includes("<FolderOpen size={13} fill='currentColor' className='flex-shrink-0 text-t-tertiary' />")).toBe(true);
  });
});
