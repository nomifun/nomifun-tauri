/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { SkillInfo } from '@/renderer/pages/settings/PresetSettings/types';
import { describe, expect, test } from 'bun:test';
import { resolveSkillDisplay } from './skillDisplay';

const skill: SkillInfo = {
  name: 'mermaid',
  description:
    'Render Mermaid diagrams as SVG or ASCII art using beautiful-mermaid. Use when users need to create flowcharts.',
  location: '/tmp/builtin-skills/mermaid/SKILL.md',
  relative_location: 'mermaid/SKILL.md',
  is_custom: false,
  source: 'builtin',
  name_i18n: {
    'zh-CN': '图表绘制',
  },
  description_i18n: {
    'zh-CN': '使用 Mermaid 渲染流程图、时序图、状态图、类图或 ER 图，可输出 SVG 或终端友好的 ASCII/Unicode 图。',
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
    expect(resolveSkillDisplay(skill, 'zh').name).toBe('图表绘制');
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
