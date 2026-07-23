/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('conversation artifact wire contract', () => {
  test('uses conversation_artifact_id end-to-end and rejects legacy identifiers', () => {
    expect(source.includes("Omit<T, 'conversation_artifact_id'>")).toBe(true);
    expect(source.includes('artifact_id?: never;')).toBe(true);
    expect(source.includes('id?: never;')).toBe(true);
    expect(
      source.includes(
        'conversation_artifact_id: parseConversationArtifactId(artifact.conversation_artifact_id)'
      )
    ).toBe(true);
    expect(source.includes('conversation_artifact_id: ConversationArtifactId;')).toBe(true);
    expect(source.includes('/artifacts/${p.conversation_artifact_id}')).toBe(true);
    expect(source.includes("hasOwnProperty.call(artifact, 'id')")).toBe(true);
    expect(source.includes("hasOwnProperty.call(artifact, 'artifact_id')")).toBe(true);
    expect(
      source.includes(
        'must use conversation_artifact_id, not id or artifact_id'
      )
    ).toBe(true);
    expect(source.includes('parseArtifactId(')).toBe(false);
  });
});
