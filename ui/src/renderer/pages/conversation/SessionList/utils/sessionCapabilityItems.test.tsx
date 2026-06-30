/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { TFunction } from 'i18next';
import React from 'react';

import { CAPABILITY_COLORS } from '@/renderer/components/capability/CapabilityIcon';
import { AUTOWORK_STATUS_COLOR, IDMM_STATUS_COLOR } from '@/renderer/components/capability/capabilityStatusColors';
import { renderIdmmCapabilityIcon } from '@/renderer/components/capability/idmmCapabilityIcon';

import { buildSessionCapabilityItems, type SessionCronStatus } from './sessionCapabilityItems';

const t = ((key: string) => key) as TFunction;

describe('buildSessionCapabilityItems', () => {
  test.each(['active', 'paused', 'unread'] as SessionCronStatus[])(
    'uses the brand-lit colour for bound cron sessions in %s state',
    (cronStatus) => {
      const cronItem = buildSessionCapabilityItems(t, { cronStatus }).find((item) => item.key === 'cron');

      expect(cronItem?.color).toBe(CAPABILITY_COLORS.brand);
    }
  );

  test('keeps cron error state in the danger colour', () => {
    const cronItem = buildSessionCapabilityItems(t, { cronStatus: 'error' }).find((item) => item.key === 'cron');

    expect(cronItem?.color).toBe(CAPABILITY_COLORS.danger);
  });

  test('uses the shared smart-decision icon for IDMM markers', () => {
    const idmmItem = buildSessionCapabilityItems(t, { idmmState: 'armed' }).find((item) => item.key === 'idmm');
    const sharedIcon = renderIdmmCapabilityIcon({ size: 13 });

    expect(React.isValidElement(idmmItem?.icon)).toBe(true);
    expect((idmmItem?.icon as React.ReactElement).type).toBe(sharedIcon.type);
  });

  // The sidebar icon and the conversation-header control must read the SAME
  // per-capability state→colour map, so the two surfaces never drift (the bug
  // where IDMM `off` resolved to blue in the sidebar but gray in the header).
  test('colours AutoWork icon from the shared map (active→active, idle→idle)', () => {
    const active = buildSessionCapabilityItems(t, { autoworkState: 'active' }).find((i) => i.key === 'autowork');
    const idle = buildSessionCapabilityItems(t, { autoworkState: 'idle' }).find((i) => i.key === 'autowork');
    expect(active?.color).toBe(AUTOWORK_STATUS_COLOR.active);
    expect(idle?.color).toBe(AUTOWORK_STATUS_COLOR.idle);
  });

  test('colours IDMM icon from the shared map (intervening→active, armed→armed)', () => {
    const intervening = buildSessionCapabilityItems(t, { idmmState: 'intervening' }).find((i) => i.key === 'idmm');
    const armed = buildSessionCapabilityItems(t, { idmmState: 'armed' }).find((i) => i.key === 'idmm');
    expect(intervening?.color).toBe(IDMM_STATUS_COLOR.intervening);
    expect(armed?.color).toBe(IDMM_STATUS_COLOR.armed);
  });

  test('routes IDMM via the shared map so off resolves to off (not the old sidebar-only primary)', () => {
    const item = buildSessionCapabilityItems(t, { idmmState: 'off' }).find((i) => i.key === 'idmm');
    expect(item?.color).toBe(IDMM_STATUS_COLOR.off);
    expect(item?.color).toBe(CAPABILITY_COLORS.off);
  });

  test('spins the IDMM icon while intervening so the session row visibly reflects it (matches the header)', () => {
    const cls = (state: 'armed' | 'intervening') => {
      const item = buildSessionCapabilityItems(t, { idmmState: state }).find((i) => i.key === 'idmm');
      return ((item?.icon as React.ReactElement).props as { className?: string }).className ?? '';
    };
    expect(cls('intervening').includes('autowork-spin')).toBe(true);
    expect(cls('armed').includes('autowork-spin')).toBe(false);
  });
});
