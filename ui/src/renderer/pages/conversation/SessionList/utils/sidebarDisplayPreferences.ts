/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type SidebarDisplayPreset = 'compact' | 'balanced' | 'detailed' | 'custom';
export type WorkpathNameMode = 'compressed' | 'folder' | 'full' | 'folderWithPath';
export type SessionMetaMode = 'none' | 'age';

export type SidebarDisplayPreferences = {
  preset: SidebarDisplayPreset;
  workpathNameMode: WorkpathNameMode;
  showGitBranch: boolean;
  sessionMetaMode: SessionMetaMode;
};

export type WorkpathDisplay =
  | {
      kind: 'compressed';
      tooltip: string;
    }
  | {
      kind: 'single';
      primary: string;
      tooltip: string;
    }
  | {
      kind: 'twoLine';
      primary: string;
      secondary: string;
      tooltip: string;
    };

const PRESET_PREFERENCES: Record<Exclude<SidebarDisplayPreset, 'custom'>, SidebarDisplayPreferences> = {
  compact: {
    preset: 'compact',
    workpathNameMode: 'folder',
    showGitBranch: false,
    sessionMetaMode: 'none',
  },
  balanced: {
    preset: 'balanced',
    workpathNameMode: 'compressed',
    showGitBranch: true,
    sessionMetaMode: 'age',
  },
  detailed: {
    preset: 'detailed',
    workpathNameMode: 'folderWithPath',
    showGitBranch: true,
    sessionMetaMode: 'age',
  },
};

export const DEFAULT_SIDEBAR_DISPLAY_PREFERENCES: SidebarDisplayPreferences = PRESET_PREFERENCES.detailed;

const PRESET_VALUES = new Set<SidebarDisplayPreset>(['compact', 'balanced', 'detailed', 'custom']);
const WORKPATH_NAME_VALUES = new Set<WorkpathNameMode>(['compressed', 'folder', 'full', 'folderWithPath']);
const SESSION_META_VALUES = new Set<SessionMetaMode>(['none', 'age']);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isSidebarDisplayPreset(value: unknown): value is SidebarDisplayPreset {
  return typeof value === 'string' && PRESET_VALUES.has(value as SidebarDisplayPreset);
}

function isWorkpathNameMode(value: unknown): value is WorkpathNameMode {
  return typeof value === 'string' && WORKPATH_NAME_VALUES.has(value as WorkpathNameMode);
}

function isSessionMetaMode(value: unknown): value is SessionMetaMode {
  return typeof value === 'string' && SESSION_META_VALUES.has(value as SessionMetaMode);
}

export function getPresetSidebarDisplayPreferences(
  preset: Exclude<SidebarDisplayPreset, 'custom'>
): SidebarDisplayPreferences {
  return { ...PRESET_PREFERENCES[preset] };
}

export function normalizeSidebarDisplayPreferences(raw: unknown): SidebarDisplayPreferences {
  if (!isRecord(raw)) return { ...DEFAULT_SIDEBAR_DISPLAY_PREFERENCES };

  const preset = raw.preset;
  if (!isSidebarDisplayPreset(preset)) return { ...DEFAULT_SIDEBAR_DISPLAY_PREFERENCES };
  if (preset !== 'custom') return getPresetSidebarDisplayPreferences(preset);

  if (
    !isWorkpathNameMode(raw.workpathNameMode) ||
    typeof raw.showGitBranch !== 'boolean' ||
    !isSessionMetaMode(raw.sessionMetaMode)
  ) {
    return { ...DEFAULT_SIDEBAR_DISPLAY_PREFERENCES };
  }

  return {
    preset,
    workpathNameMode: raw.workpathNameMode,
    showGitBranch: raw.showGitBranch,
    sessionMetaMode: raw.sessionMetaMode,
  };
}

export function withCustomSidebarDisplayPreference(
  preferences: SidebarDisplayPreferences,
  patch: Partial<Omit<SidebarDisplayPreferences, 'preset'>>
): SidebarDisplayPreferences {
  return normalizeSidebarDisplayPreferences({
    ...preferences,
    ...patch,
    preset: 'custom',
  });
}

function getPathParts(path: string): string[] {
  return path.split(/[\\/]+/).filter(Boolean);
}

function parentPath(path: string): string {
  const normalized = path.replace(/[\\/]+$/, '');
  const separatorMatch = normalized.match(/[\\/]/g);
  const separator = separatorMatch?.at(-1) ?? '/';
  const index = normalized.lastIndexOf(separator);
  if (index <= 0) return '';
  return normalized.slice(0, index);
}

export function formatWorkpathDisplay(
  workpath: string,
  displayName: string,
  mode: WorkpathNameMode
): WorkpathDisplay {
  if (mode === 'compressed') {
    return { kind: 'compressed', tooltip: workpath };
  }

  const folder = displayName || getPathParts(workpath).at(-1) || workpath;
  if (mode === 'folder') {
    return { kind: 'single', primary: folder, tooltip: workpath };
  }
  if (mode === 'full') {
    return { kind: 'single', primary: workpath, tooltip: workpath };
  }
  return {
    kind: 'twoLine',
    primary: folder,
    secondary: parentPath(workpath),
    tooltip: workpath,
  };
}
