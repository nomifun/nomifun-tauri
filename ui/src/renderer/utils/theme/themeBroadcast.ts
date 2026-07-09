/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { Theme } from '@renderer/hooks/system/useTheme';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';

/**
 * Cross-window light/dark theme sync.
 *
 * Each Tauri window is its own webview with its own `configService` instance,
 * and `configService.subscribe` only fires for same-window writes — so toggling
 * the theme in the main window does NOT reach the always-on-top desktop-companion
 * window. We bridge that gap with a Tauri global event: the setter that changes
 * the theme broadcasts the new value, and standalone windows (the companion)
 * listen and re-apply `data-theme` live.
 *
 * No-op outside the desktop shell (web/WebUI runs a single document, nothing to sync).
 */
export const THEME_SYNC_EVENT = 'nomifun://theme-sync';

export interface ThemeSyncPayload {
  /** Light/dark scheme — present on a theme toggle, absent on a customCss-only sync. */
  theme?: Theme;
  /** Active ambiance preset CSS — present on a customCss change, absent on a light/dark-only sync. */
  customCss?: string;
}

/** Broadcast a light/dark theme change to every window (incl. the companion). */
export function broadcastThemeSync(theme: Theme): void {
  if (!isTauriRuntime()) return;
  void import('@tauri-apps/api/event')
    .then(({ emit }) => emit(THEME_SYNC_EVENT, { theme } satisfies ThemeSyncPayload))
    .catch(() => {
      /* best-effort; a missed broadcast just means the other window updates on next reload */
    });
}

/**
 * Broadcast an ambiance-preset (customCss) change to every window. Standalone
 * windows that don't mount Layout (the desktop companion) listen and re-inject
 * the CSS live, so their bubble/input chrome tracks the main window's theme.
 * Rides the same THEME_SYNC_EVENT channel with a distinct payload field.
 */
export function broadcastCustomCssSync(customCss: string): void {
  if (!isTauriRuntime()) return;
  void import('@tauri-apps/api/event')
    .then(({ emit }) => emit(THEME_SYNC_EVENT, { customCss } satisfies ThemeSyncPayload))
    .catch(() => {
      /* best-effort; the companion re-reads customCss from config on its next load */
    });
}
