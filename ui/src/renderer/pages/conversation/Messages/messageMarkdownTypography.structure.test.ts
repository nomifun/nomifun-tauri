/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const markdownMessageFiles = [
  './acp/MessageAcpToolCall.tsx',
  './components/MessageText.tsx',
  './components/MessageTips.tsx',
  './components/MessageToolGroup.tsx',
  './components/SkillSuggestCard.tsx',
];

describe('message markdown typography', () => {
  test('uses the shared message body typography for every message MarkdownView', () => {
    for (const filePath of markdownMessageFiles) {
      const source = readFileSync(new URL(filePath, import.meta.url), 'utf8');
      const markdownTags = source.match(/<MarkdownView\b[^>]*>/g) ?? [];

      expect(markdownTags.length > 0).toBe(true);
      for (const tag of markdownTags) {
        expect(tag.includes('fontSize={MESSAGE_BODY_FONT_SIZE}')).toBe(true);
        expect(tag.includes('lineHeight={MESSAGE_BODY_LINE_HEIGHT}')).toBe(true);
      }
    }
  });
});
