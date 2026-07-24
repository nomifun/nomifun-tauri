/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { TChatConversation } from '@/common/config/storage';
import {
  parsePresetReference,
  type PresetReference,
} from '@/common/types/agent/presetTypes';

const PRESET_SELECTION_PREFIX = 'preset:';

/**
 * The selected key is the durable launch intent. Catalog records enrich that
 * intent, but a refresh must never silently turn it into a bare-agent launch.
 */
export function presetIdFromSelectionKey(selectedAgentKey: string): PresetReference | undefined {
  if (!selectedAgentKey.startsWith(PRESET_SELECTION_PREFIX)) return undefined;
  return parsePresetReference(selectedAgentKey.slice(PRESET_SELECTION_PREFIX.length));
}

/**
 * A preset launch is successful only when the create response proves that the
 * exact requested immutable snapshot was persisted. Call this before initial
 * message handoff so a broken lineage can never run as a bare conversation.
 */
export function assertCreatedConversationPreset(
  conversation: TChatConversation,
  expectedPresetId: PresetReference | undefined,
): void {
  if (!expectedPresetId) return;

  const snapshot = conversation.preset_snapshot;
  if (conversation.preset_id !== expectedPresetId) {
    throw new TypeError(
      `Created conversation preset mismatch: expected ${expectedPresetId}, received ${conversation.preset_id ?? 'none'}`,
    );
  }
  if (!Number.isSafeInteger(conversation.preset_revision) || (conversation.preset_revision ?? 0) <= 0) {
    throw new TypeError('Created preset conversation is missing a valid preset_revision');
  }
  if (!snapshot) {
    throw new TypeError('Created preset conversation is missing preset_snapshot');
  }
  if (snapshot.preset_id !== expectedPresetId) {
    throw new TypeError('Created conversation preset_snapshot has a mismatched preset_id');
  }
  if (snapshot.preset_revision !== conversation.preset_revision) {
    throw new TypeError('Created conversation preset_snapshot has a mismatched preset_revision');
  }
}
