/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { conversationTarget, terminalTarget } from '@/common/types/ids';
import { resolveWorkspaceCollapseAfterHasFiles } from './useWorkspaceCollapse';

describe('resolveWorkspaceCollapseAfterHasFiles', () => {
  const conversation = conversationTarget('0190f5fe-7c00-7a00-8000-000000000042');

  test('keeps the conversation workspace collapsed when file signals are not allowed to auto-expand it', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { target: conversation, hasFiles: true, isInitial: true },
        isMobile: false,
        autoExpandOnFiles: false,
        isTemporaryWorkspace: false,
        userPreference: null,
        target: conversation,
      })
    ).toBe(true);
  });

  test('keeps temporary conversation workspaces collapsed when files appear mid-session', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { target: conversation, hasFiles: true, isInitial: false },
        isMobile: false,
        autoExpandOnFiles: false,
        isTemporaryWorkspace: true,
        userPreference: null,
        target: conversation,
      })
    ).toBe(true);
  });

  test('still respects explicit user expansion even when file auto-expand is disabled', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { target: conversation, hasFiles: true, isInitial: true },
        isMobile: false,
        autoExpandOnFiles: false,
        isTemporaryWorkspace: false,
        userPreference: 'expanded',
        target: conversation,
      })
    ).toBe(false);
  });

  test('preserves terminal-style auto-expand when enabled explicitly', () => {
    const terminal = terminalTarget('0190f5fe-7c00-7a00-8000-000000000001');
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: { target: terminal, hasFiles: true, isInitial: true },
        isMobile: false,
        autoExpandOnFiles: true,
        isTemporaryWorkspace: false,
        userPreference: null,
        target: terminal,
      })
    ).toBe(false);
  });

  test('ignores file signals from other workspace rails', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: {
          target: conversationTarget('0190f5fe-7c00-7a00-8000-000000000043'),
          hasFiles: true,
          isInitial: true,
        },
        isMobile: false,
        autoExpandOnFiles: true,
        isTemporaryWorkspace: false,
        userPreference: null,
        target: conversation,
      })
    ).toBe(true);
  });

  test('does not cross conversation and terminal namespaces for the same legacy id', () => {
    expect(
      resolveWorkspaceCollapseAfterHasFiles({
        currentCollapsed: true,
        detail: {
          target: terminalTarget('0190f5fe-7c00-7a00-8000-000000000042'),
          hasFiles: true,
          isInitial: true,
        },
        isMobile: false,
        autoExpandOnFiles: true,
        isTemporaryWorkspace: false,
        userPreference: null,
        target: conversation,
      })
    ).toBe(true);
  });
});
