/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parsePresetReference } from '@/common/types/agent/presetTypes';
import {
  assertCreatedConversationPreset,
  presetIdFromSelectionKey,
} from './presetConversationContract';

const presetId = parsePresetReference('0190f5fe-7c00-7a00-8000-000000000011');
const otherPresetId = parsePresetReference('0190f5fe-7c00-7a00-8000-000000000012');

const thrownMessage = (run: () => unknown): string => {
  try {
    run();
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
  throw new Error('expected function to throw');
};

const conversation = (overrides: Partial<TChatConversation> = {}) =>
  ({
    preset_id: presetId,
    preset_revision: 3,
    preset_snapshot: {
      preset_id: presetId,
      preset_revision: 3,
    },
    ...overrides,
  }) as TChatConversation;

describe('durable Guid preset launch contract', () => {
  test('parses the selected preset key independently of catalog metadata', () => {
    expect(presetIdFromSelectionKey(`preset:${presetId}`)).toBe(presetId);
    expect(presetIdFromSelectionKey('nomi')).toBeUndefined();
    expect(thrownMessage(() => presetIdFromSelectionKey('preset:legacy-id')).length > 0).toBe(true);
  });

  test('accepts only the exact persisted preset snapshot', () => {
    assertCreatedConversationPreset(conversation(), presetId);
    expect(
      thrownMessage(() =>
        assertCreatedConversationPreset(conversation({ preset_id: otherPresetId }), presetId),
      ).includes('preset mismatch'),
    ).toBe(true);
    expect(
      thrownMessage(() =>
        assertCreatedConversationPreset(conversation({ preset_snapshot: undefined }), presetId),
      ).includes('missing preset_snapshot'),
    ).toBe(true);
    expect(
      thrownMessage(() =>
        assertCreatedConversationPreset(
          conversation({
            preset_snapshot: {
              preset_id: otherPresetId,
              preset_revision: 3,
            } as TChatConversation['preset_snapshot'],
          }),
          presetId,
        ),
      ).includes('mismatched preset_id'),
    ).toBe(true);
    expect(
      thrownMessage(() =>
        assertCreatedConversationPreset(
          conversation({
            preset_snapshot: {
              preset_id: presetId,
              preset_revision: 4,
            } as TChatConversation['preset_snapshot'],
          }),
          presetId,
        ),
      ).includes('mismatched preset_revision'),
    ).toBe(true);
  });

  test('does not impose preset lineage on bare-agent launches', () => {
    assertCreatedConversationPreset(
      conversation({
        preset_id: undefined,
        preset_revision: undefined,
        preset_snapshot: undefined,
      }),
      undefined,
    );
  });
});
