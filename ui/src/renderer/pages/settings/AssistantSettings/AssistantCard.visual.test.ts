/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./AssistantCard.tsx', import.meta.url), 'utf8');

describe('AssistantCard visual hierarchy', () => {
  test('uses a borderless neutral card surface instead of a theme-coloured outline', () => {
    expect(source.includes("'group relative flex flex-col rounded-16px p-14px cursor-pointer'")).toBe(true);
    expect(source.includes('min-h-[214px]')).toBe(false);
    expect(source.includes("'border-[var(--color-border-2)] bg-[var(--color-bg-2)]")).toBe(false);
    expect(source.includes('hover:border-[var(--color-primary-light-4)]')).toBe(false);
  });

  test('pins the text actions to the bottom and keeps their icon and label centered without a separator', () => {
    expect(source.includes('mt-auto pt-12px flex min-h-36px items-center justify-end gap-12px')).toBe(true);
    expect(source.includes('border-t border-solid')).toBe(false);
    expect(source.includes('inline-flex items-center gap-4px leading-none text-12px')).toBe(true);
  });

  test('lets short descriptions use their natural height while clamping overflow at two lines', () => {
    expect(source.includes('WebkitLineClamp: 2')).toBe(true);
    expect(source.includes('min-h-[36px]')).toBe(false);
  });
});
