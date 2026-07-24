/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import { describe, expect, test } from 'bun:test';
import { resolveSkillDisplay } from './skillDisplay';

const skill: SkillInfo = {
  name: 'planning-with-files',
  description:
    'Maintain durable task plans, findings, and progress files for long-running work.',
  location: '/tmp/builtin-skills/planning-with-files/SKILL.md',
  relative_location: 'planning-with-files/SKILL.md',
  is_custom: false,
  source: 'builtin',
  name_i18n: {
    'zh-CN': '文件化规划',
  },
  description_i18n: {
    'zh-CN': '为长期任务维护持久的计划、发现与进度文件。',
  },
};

describe('skill display localization', () => {
  test('uses localized built-in skill descriptions for the active locale', () => {
    expect(resolveSkillDisplay(skill, 'zh-CN').description).toBe(skill.description_i18n?.['zh-CN']);
  });

  test('falls back to the canonical skill description when locale metadata is missing', () => {
    expect(resolveSkillDisplay(skill, 'en-US').description).toBe(skill.description);
  });

  test('uses the same resolver for localized names and language-family locale variants', () => {
    expect(resolveSkillDisplay(skill, 'zh').name).toBe('文件化规划');
    expect(resolveSkillDisplay(skill, 'ZH-hans').description).toBe(skill.description_i18n?.['zh-CN']);
  });

  test('supports lightweight auto-injected skill records without SkillInfo-only fields', () => {
    expect(
      resolveSkillDisplay(
        {
          name: 'cron',
          description: 'Scheduled task management.',
          description_i18n: { 'zh-CN': '定时任务管理。' },
        },
        'zh-CN'
      )
    ).toEqual({
      name: 'cron',
      description: '定时任务管理。',
    });
  });
});
