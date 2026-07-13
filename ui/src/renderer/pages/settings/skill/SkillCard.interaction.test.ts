/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

describe('SkillCard interaction ownership', () => {
  test('opens details from the card and reserves tag editing for its explicit action', () => {
    const source = readFileSync(new URL('./SkillCard.tsx', import.meta.url), 'utf8');
    const cardClick = source.indexOf('onClick={() => onOpenDetails(skill)}');
    const footerStop = source.indexOf('onClick={(e) => e.stopPropagation()}');
    const tagClick = source.indexOf('onClick={() => onEditTags(skill)}');

    expect(cardClick).toBeGreaterThanOrEqual(0);
    expect(footerStop).toBeGreaterThan(cardClick);
    expect(tagClick).toBeGreaterThan(footerStop);
    expect(source.includes('e.stopPropagation();\n              onEditTags(skill);')).toBe(true);
    expect(source.includes('border-t border-solid border-[var(--color-border-1)]')).toBe(false);
    expect(source.includes('absolute bottom-14px right-14px flex items-center justify-end')).toBe(true);
    expect(source.includes('pb-42px cursor-pointer outline-none')).toBe(true);
    expect(source.includes('text-12px leading-none text-[var(--color-text-3)] cursor-pointer hover:text-[var(--color-text-2)]')).toBe(true);
    expect(source.includes("<SettingOne theme='outline' size={13} strokeWidth={3} fill='currentColor' className='relative top-px shrink-0' />")).toBe(
      true
    );
    expect(source.includes("<span className='leading-none'>{t('settings.skillsHub.editTags'")).toBe(true);
  });
});
