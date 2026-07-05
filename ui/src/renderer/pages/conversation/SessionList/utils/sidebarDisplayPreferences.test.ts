/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import {
  DEFAULT_SIDEBAR_DISPLAY_PREFERENCES,
  formatWorkpathDisplay,
  getPresetSidebarDisplayPreferences,
  normalizeSidebarDisplayPreferences,
} from './sidebarDisplayPreferences';

describe('sidebarDisplayPreferences', () => {
  test('defaults to the detailed preset with parent path, branch badges, and age metadata', () => {
    expect(DEFAULT_SIDEBAR_DISPLAY_PREFERENCES).toEqual({
      preset: 'detailed',
      workpathNameMode: 'folderWithPath',
      showGitBranch: true,
      sessionMetaMode: 'age',
    });
  });

  test('maps compact, balanced, and detailed presets to concrete display strategies', () => {
    expect(getPresetSidebarDisplayPreferences('compact')).toMatchObject({
      workpathNameMode: 'folder',
      showGitBranch: false,
      sessionMetaMode: 'none',
    });
    expect(getPresetSidebarDisplayPreferences('balanced')).toMatchObject({
      workpathNameMode: 'compressed',
      showGitBranch: true,
      sessionMetaMode: 'age',
    });
    expect(getPresetSidebarDisplayPreferences('detailed')).toMatchObject({
      workpathNameMode: 'folderWithPath',
      showGitBranch: true,
      sessionMetaMode: 'age',
    });
  });

  test('normalizes invalid persisted data back to the detailed defaults', () => {
    expect(normalizeSidebarDisplayPreferences(null)).toEqual(DEFAULT_SIDEBAR_DISPLAY_PREFERENCES);
    expect(normalizeSidebarDisplayPreferences({ preset: 'nope', workpathNameMode: 123 })).toEqual(
      DEFAULT_SIDEBAR_DISPLAY_PREFERENCES
    );
  });

  test('keeps custom strategies when individual options diverge from presets', () => {
    expect(
      normalizeSidebarDisplayPreferences({
        preset: 'custom',
        workpathNameMode: 'full',
        showGitBranch: false,
        sessionMetaMode: 'age',
      })
    ).toEqual({
      preset: 'custom',
      workpathNameMode: 'full',
      showGitBranch: false,
      sessionMetaMode: 'age',
    });
  });

  test('formats workpath labels for folder, full path, and two-line strategies', () => {
    const path = '/Users/muri/code/nomifun/nomifun-tauri';

    expect(formatWorkpathDisplay(path, 'nomifun-tauri', 'folder')).toEqual({
      kind: 'single',
      primary: 'nomifun-tauri',
      tooltip: path,
    });
    expect(formatWorkpathDisplay(path, 'nomifun-tauri', 'full')).toEqual({
      kind: 'single',
      primary: path,
      tooltip: path,
    });
    expect(formatWorkpathDisplay(path, 'nomifun-tauri', 'folderWithPath')).toEqual({
      kind: 'twoLine',
      primary: 'nomifun-tauri',
      secondary: '/Users/muri/code/nomifun',
      tooltip: path,
    });
  });
});
