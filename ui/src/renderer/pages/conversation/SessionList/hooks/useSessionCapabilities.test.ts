/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import type { IIdmmState } from '@/common/adapter/ipcBridge';
import { parseConversationId } from '@/common/types/ids';

import {
  applyIdmmStateToSessionCapabilities,
  capabilityKey,
  getSessionCapabilitySnapshot,
  resetSessionCapabilitiesForTest,
} from './useSessionCapabilities';

const idmmState = (overrides: Partial<IIdmmState> = {}): IIdmmState => ({
  kind: 'conversation',
  target_id: parseConversationId('0190f5fe-7c00-7a00-8000-000000000007'),
  enabled: true,
  run_state: 'armed',
  interventions_count: 0,
  sidecar_provider_resolved: true,
  ...overrides,
});

describe('SessionList capability snapshot', () => {
  const conversationId = parseConversationId('0190f5fe-7c00-7a00-8000-000000000007');

  test('applies an enabled IDMM state returned from the control save flow', () => {
    resetSessionCapabilitiesForTest();

    applyIdmmStateToSessionCapabilities(idmmState());

    const snapshot = getSessionCapabilitySnapshot();
    expect(snapshot.idmm.get(capabilityKey('conversation', conversationId))).toBe('armed');
  });

  test('removes IDMM state when the control save flow disables it', () => {
    resetSessionCapabilitiesForTest();
    applyIdmmStateToSessionCapabilities(idmmState());

    applyIdmmStateToSessionCapabilities(idmmState({ enabled: false, run_state: 'off' }));

    const snapshot = getSessionCapabilitySnapshot();
    expect(snapshot.idmm.has(capabilityKey('conversation', conversationId))).toBe(false);
  });
});
