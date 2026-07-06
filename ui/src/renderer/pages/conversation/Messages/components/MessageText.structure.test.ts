/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./MessageText.tsx', import.meta.url), 'utf8');
const typographySource = readFileSync(new URL('../typography.ts', import.meta.url), 'utf8');

describe('MessageText process action chrome', () => {
  test('can hide the hover copy and timestamp row for process text', () => {
    expect(source.includes('hideActions?: boolean')).toBe(true);
    expect(source.includes('const shouldShowActions = !hideActions && !isMobile;')).toBe(true);
    expect(source.includes('{shouldShowActions && (')).toBe(true);
  });

  test('uses one body typography contract for plain text and markdown text', () => {
    expect(typographySource.includes("export const MESSAGE_BODY_FONT_SIZE = 'var(--conversation-message-font-size)';")).toBe(
      true
    );
    expect(
      typographySource.includes("export const MESSAGE_BODY_LINE_HEIGHT = 'var(--conversation-message-line-height)';")
    ).toBe(true);
    expect(typographySource.includes("export const MESSAGE_BODY_CLASS_NAME = 'message-text-body whitespace-pre-wrap break-words';")).toBe(
      true
    );
    expect(source.includes("from '../typography'")).toBe(true);
    expect(source.includes('className={MESSAGE_BODY_CLASS_NAME}')).toBe(true);
    expect(source.includes('fontSize={MESSAGE_BODY_FONT_SIZE}')).toBe(true);
    expect(source.includes('lineHeight={MESSAGE_BODY_LINE_HEIGHT}')).toBe(true);
  });
});
