/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { PersistedArtifactId } from './ids';
import {
  parseConversationArtifactId,
  type ConversationArtifactId,
} from './conversationArtifact';

describe('Conversation Artifact identity', () => {
  test('accepts only a canonical bare lowercase UUIDv7', () => {
    const valid = '0190f5fe-7c00-7a00-8abc-012345678951';
    expect(parseConversationArtifactId(valid)).toBe(valid);

    for (const invalid of [
      42,
      `artifact_${valid}`,
      valid.toUpperCase(),
      `${valid} `,
      '550e8400-e29b-41d4-a716-446655440000',
    ]) {
      let rejected = false;
      try {
        parseConversationArtifactId(invalid);
      } catch {
        rejected = true;
      }
      expect(rejected).toBe(true);
    }
  });

  test('is not assignable to a persisted tool artifact receipt ID', () => {
    const conversationArtifactId = parseConversationArtifactId(
      '0190f5fe-7c00-7a00-8abc-012345678951'
    );
    const retainConversationType: ConversationArtifactId = conversationArtifactId;
    expect(retainConversationType).toBe(conversationArtifactId);

    // @ts-expect-error Conversation Artifact rows and persisted tool receipts
    // are distinct product entities even though both use UUIDv7.
    const persistedArtifactId: PersistedArtifactId = conversationArtifactId;
    expect(persistedArtifactId).toBe(conversationArtifactId);
  });
});
