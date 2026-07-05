/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { resolveWorkspaceCollapseAfterHasFiles } from './useWorkspaceCollapse';

describe('resolveWorkspaceCollapseAfterHasFiles', () => {
  test('keeps the conversation workspace collapsed when file signals are not allowed to auto-expand it', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { hasFiles: true, conversation_id: '42', isInitial: true },
        isMobile: false,
        autoExpandOnFiles: false,
        isTemporaryWorkspace: false,
        userPreference: null,
      })
    ).toBe(true);
  });

  test('keeps temporary conversation workspaces collapsed when files appear mid-session', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { hasFiles: true, conversation_id: '42', isInitial: false },
        isMobile: false,
        autoExpandOnFiles: false,
        isTemporaryWorkspace: true,
        userPreference: null,
      })
    ).toBe(true);
  });

  test('still respects explicit user expansion even when file auto-expand is disabled', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { hasFiles: true, conversation_id: '42', isInitial: true },
        isMobile: false,
        autoExpandOnFiles: false,
        isTemporaryWorkspace: false,
        userPreference: 'expanded',
      })
    ).toBe(false);
  });

  test('preserves terminal-style auto-expand when enabled explicitly', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { hasFiles: true, conversation_id: 'terminal-1', isInitial: true },
        isMobile: false,
        autoExpandOnFiles: true,
        isTemporaryWorkspace: false,
        userPreference: null,
      })
    ).toBe(false);
  });

  test('ignores file signals from other workspace rails', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { hasFiles: true, conversation_id: 'other-workspace', isInitial: true },
        isMobile: false,
        autoExpandOnFiles: true,
        isTemporaryWorkspace: false,
        userPreference: null,
        workspaceEventKey: 'current-workspace',
      })
    ).toBe(true);
  });

  test('ignores unscoped file signals when the rail has a workspace event key', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { hasFiles: true, isInitial: true },
        isMobile: false,
        autoExpandOnFiles: true,
        isTemporaryWorkspace: false,
        userPreference: null,
        workspaceEventKey: 'current-workspace',
      })
    ).toBe(true);
  });
});
