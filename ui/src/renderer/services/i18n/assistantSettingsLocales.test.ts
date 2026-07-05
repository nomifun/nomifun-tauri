/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import enSettings from './locales/en-US/settings.json';
import zhSettings from './locales/zh-CN/settings.json';

const REQUIRED_ASSISTANT_SKILLS_HUB_KEYS = ['title', 'subtitle', 'railTitle', 'assistantsTab', 'skillsTab'] as const;

const assertAssistantSettingsLocale = (settings: Record<string, unknown>) => {
  expect(typeof settings.assistantSkills).toBe('string');
  expect((settings.assistantSkills as string).trim()).toBeTruthy();

  expect(settings.assistantSkillsHub).toBeDefined();
  expect(typeof settings.assistantSkillsHub).toBe('object');
  for (const key of REQUIRED_ASSISTANT_SKILLS_HUB_KEYS) {
    const value = (settings.assistantSkillsHub as Record<string, unknown> | undefined)?.[key];
    expect(typeof value).toBe('string');
    expect((value as string).trim()).toBeTruthy();
  }
};

describe('assistant settings locale coverage', () => {
  test('en-US keeps editor skill label separate from the assistant/skill hub strings', () => {
    assertAssistantSettingsLocale(enSettings);
  });

  test('zh-CN keeps editor skill label separate from the assistant/skill hub strings', () => {
    assertAssistantSettingsLocale(zhSettings);
  });
});
