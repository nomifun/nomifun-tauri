/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('public contact links', () => {
  test('keeps About and Contact surfaces wired to current public channels', () => {
    const aboutSource = readSource(new URL('./AboutModalContent.tsx', import.meta.url));
    const contactSource = readSource(new URL('./FeedbackReportModal.tsx', import.meta.url));
    const combined = `${aboutSource}\n${contactSource}`;

    for (const target of [
      'https://www.nomifun.com',
      'https://www.nomifun.com/contact',
      'https://github.com/nomifun/nomifun-tauri/issues',
      'https://github.com/nomifun/nomifun-tauri/releases',
      '535526063@qq.com',
    ]) {
      expect(combined.includes(target)).toBe(true);
    }

    expect(aboutSource.includes('ABOUT_LINK_TARGET')).toBe(false);
  });

  test('keeps the Contact modal visually quiet instead of rendering chunky cards', () => {
    const contactSource = readSource(new URL('./FeedbackReportModal.tsx', import.meta.url));

    expect(contactSource.includes("import CopyIconButton from '@/renderer/components/base/CopyIconButton'")).toBe(true);
    expect(contactSource.includes("<Info theme='outline' size='28' />")).toBe(false);
    expect(contactSource.includes("bg-fill-2 px-12px py-10px")).toBe(false);
    expect(contactSource.includes('>↗<')).toBe(false);
  });
});
