/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

import enConversation from '../../../services/i18n/locales/en-US/conversation.json';
import zhConversation from '../../../services/i18n/locales/zh-CN/conversation.json';

const source = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

describe('SendBox slash command localization', () => {
  test('keeps /compact command description in locale files and renders it through i18n', () => {
    expect(zhConversation.slashCommands?.compact?.description).toBe('压缩会话上下文');
    expect(enConversation.slashCommands?.compact?.description).toBe('Compress conversation context');

    expect(source.includes("command.name === 'compact'")).toBe(true);
    expect(source.includes('conversation.slashCommands.compact.description')).toBe(true);
    expect(source.includes('description: getSlashCommandDescription(command, t)')).toBe(true);
  });
});
