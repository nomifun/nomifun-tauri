/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ICssTheme } from '@/common/config/storage.ts';

// Theme CSS loaded as raw strings via Vite ?raw imports
import rhythmDarkCss from './presets/rhythm-dark.css?raw';
import codexNeutralCss from './presets/codex-neutral.css?raw';
import neonRainbowCss from './presets/neon-rainbow.css?raw';
import frostedGlassCss from './presets/frosted-glass.css?raw';
import sunsetAfterglowCss from './presets/sunset-afterglow.css?raw';

/**
 * 系统默认主题 ID / System default theme ID
 * 无显式选择时（空 activeThemeId）回退并应用此主题；也是主题缺失时的兜底。
 * Applied when no theme is explicitly selected (empty activeThemeId); also the fallback when a theme is missing.
 */
export const DEFAULT_THEME_ID = 'codex-neutral';

/**
 * 预设 CSS 主题列表 / Preset CSS themes list
 * 这些主题是内置的，用户可以直接选择使用 / These themes are built-in and can be directly used by users
 * 新增主题请遵循 presets/README.md 的主题契约 / New themes must follow the contract in presets/README.md
 * 数组顺序 = 卡片展示顺序：默认主题「经典」置首。
 * Array order = card display order: the default "Classic" first.
 */
export const PRESET_THEMES: ICssTheme[] = [
  {
    id: DEFAULT_THEME_ID,
    name: '经典',
    is_preset: true,
    css: codexNeutralCss,
    created_at: Date.now(),
    updated_at: Date.now(),
  },
  {
    id: 'rhythm-dark',
    name: '律动暗黑 Rhythm Dark',
    is_preset: true,
    css: rhythmDarkCss,
    created_at: Date.now(),
    updated_at: Date.now(),
  },
  {
    id: 'neon-rainbow',
    name: '暗夜霓虹 Neon Night',
    is_preset: true,
    css: neonRainbowCss,
    created_at: Date.now(),
    updated_at: Date.now(),
  },
  {
    id: 'frosted-glass',
    name: '冰晶幻境 Frosted Glass',
    is_preset: true,
    css: frostedGlassCss,
    created_at: Date.now(),
    updated_at: Date.now(),
  },
  {
    id: 'sunset-afterglow',
    name: '落日余晖 Sunset Afterglow',
    is_preset: true,
    css: sunsetAfterglowCss,
    created_at: Date.now(),
    updated_at: Date.now(),
  },
];
