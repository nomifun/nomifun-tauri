/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/** Run status → theme-var color + i18n key suffix (mirrors the page's STATUS_META
 * + RunHistory). Single source of truth for the glass-header status pill. */
export const STATUS_META: Record<string, { color: string; key: string }> = {
  planning: { color: 'var(--warning)', key: 'planning' },
  running: { color: 'rgb(var(--primary-6))', key: 'running' },
  completed: { color: 'var(--success)', key: 'completed' },
  failed: { color: 'var(--danger)', key: 'failed' },
  cancelled: { color: 'var(--color-text-3)', key: 'cancelled' },
  paused: { color: 'var(--warning)', key: 'paused' },
  awaiting_plan_approval: { color: 'rgb(var(--primary-6))', key: 'awaiting_plan_approval' },
};
