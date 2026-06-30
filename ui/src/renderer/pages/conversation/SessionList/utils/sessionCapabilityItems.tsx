/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { AutoWorkRunState, IdmmRunState } from '@/common/adapter/ipcBridge';
import type { CapabilityIconItem } from '@/renderer/components/capability/CapabilityIcon';
import { CAPABILITY_COLORS } from '@/renderer/components/capability/CapabilityIcon';
import { AUTOWORK_STATUS_COLOR, IDMM_STATUS_COLOR } from '@/renderer/components/capability/capabilityStatusColors';
import { renderIdmmCapabilityIcon } from '@/renderer/components/capability/idmmCapabilityIcon';
import { AlarmClock, Robot } from '@icon-park/react';
import type { TFunction } from 'i18next';
import React from 'react';

/** Capability icon size inside the 34px session row (status dots beside it are 6px). */
export const CAPABILITY_ICON_SIZE = 13;

/** Aggregated cron status of one session ('unread' is conversation-only). */
export type SessionCronStatus = 'none' | 'active' | 'paused' | 'error' | 'unread';

export interface SessionCapabilityStates {
  cronStatus?: SessionCronStatus;
  /** AutoWork run state when enabled (undefined = not enabled / unknown). */
  autoworkState?: AutoWorkRunState;
  /** IDMM run state when enabled (undefined = not enabled / unknown). */
  idmmState?: IdmmRunState;
}

/**
 * Session-level capability markers for the trailing CapabilityIconCluster, in
 * fixed order: 定时任务 → 自动工作 → 智能决策. Shared by ConversationRow and
 * TerminalRow so both rows keep identical icons, palette, and tooltip wording.
 * Cron 'unread' carries the red badge dot.
 */
export const buildSessionCapabilityItems = (
  t: TFunction,
  { cronStatus = 'none', autoworkState, idmmState }: SessionCapabilityStates
): CapabilityIconItem[] => {
  const items: CapabilityIconItem[] = [];

  if (cronStatus !== 'none') {
    items.push({
      key: 'cron',
      icon: <AlarmClock theme='outline' size={CAPABILITY_ICON_SIZE} fill='currentColor' />,
      color: cronStatus === 'error' ? CAPABILITY_COLORS.danger : CAPABILITY_COLORS.brand,
      dot: cronStatus === 'unread',
      title: `${t('cron.scheduledTasks')} · ${t(`cron.status.${cronStatus}`)}`,
    });
  }

  if (autoworkState) {
    items.push({
      key: 'autowork',
      icon: <Robot theme='outline' size={CAPABILITY_ICON_SIZE} fill='currentColor' />,
      color: AUTOWORK_STATUS_COLOR[autoworkState],
      title: `${t('requirements.autowork.label')} · ${t(`requirements.autowork.state.${autoworkState}`)}`,
    });
  }

  if (idmmState) {
    items.push({
      key: 'idmm',
      // Spin while intervening — same animation the per-session control shows —
      // so the session row visibly reflects an in-flight IDMM intervention, not
      // just a colour change (which is brief for rule-tier answers).
      icon: renderIdmmCapabilityIcon({ size: CAPABILITY_ICON_SIZE, spinning: idmmState === 'intervening' }),
      color: IDMM_STATUS_COLOR[idmmState],
      title: `${t('idmm.label')} · ${t(`idmm.state.${idmmState}`)}`,
    });
  }

  return items;
};
