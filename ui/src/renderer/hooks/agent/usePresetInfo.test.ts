/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parsePresetReference, type Preset, type ResolvedPresetSnapshot } from '@/common/types/agent/presetTypes';
import { resolvePresetDisplayName } from './usePresetInfo';

const presetId = parsePresetReference('0190f5fe-7c00-7a00-8000-000000000011');

describe('preset conversation display identity', () => {
  test('prefers the frozen snapshot name over a renamed live catalog record', () => {
    const snapshot = {
      preset_id: presetId,
      preset_revision: 1,
      preset_name: 'Launch name',
    } as ResolvedPresetSnapshot;
    const livePreset = {
      preset_id: presetId,
      name: 'Renamed later',
      name_i18n: { 'zh-CN': '后来改名' },
    } as unknown as Preset;

    expect(resolvePresetDisplayName(presetId, snapshot, livePreset, 'zh-CN')).toBe(
      'Launch name',
    );
  });

  test('uses localized live metadata only when no immutable snapshot exists', () => {
    const livePreset = {
      preset_id: presetId,
      name: 'Live name',
      name_i18n: { 'zh-CN': '当前名称' },
    } as unknown as Preset;

    expect(resolvePresetDisplayName(presetId, null, livePreset, 'zh-CN')).toBe(
      '当前名称',
    );
  });
});
