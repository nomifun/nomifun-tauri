/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { AutoWorkRunState, IdmmRunState } from '@/common/adapter/ipcBridge';

import { CAPABILITY_COLORS } from './CapabilityIcon';

/**
 * Per-capability run-state → colour, derived from the shared {@link CAPABILITY_COLORS}
 * palette. This is the SINGLE routing table both surfaces read:
 *  - the conversation-header controls (AutoWorkControl / IdmmControl) colour their
 *    trigger icon + status marker through it, and
 *  - the session-list capability icons (sessionCapabilityItems) colour the row icon
 *    through it.
 * Keeping the state→colour mapping here (not re-inlined per surface) is what keeps
 * the header and the sidebar from drifting — the bug that had IDMM `off` resolve to
 * gray in the header but blue in the sidebar.
 */
export const AUTOWORK_STATUS_COLOR: Record<AutoWorkRunState, string> = {
  off: CAPABILITY_COLORS.off,
  idle: CAPABILITY_COLORS.idle,
  active: CAPABILITY_COLORS.active,
};

export const IDMM_STATUS_COLOR: Record<IdmmRunState, string> = {
  off: CAPABILITY_COLORS.off,
  armed: CAPABILITY_COLORS.armed,
  intervening: CAPABILITY_COLORS.active,
};
