/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('preview persistence entity isolation', () => {
  test('requires an explicit entity namespace and has no legacy fallback', () => {
    const source = readSource(new URL('./PreviewContext.tsx', import.meta.url));

    expect(source.includes('persistNamespace: string')).toBe(true);
    expect(source.includes('persistNamespace?: string')).toBe(false);
    expect(source.includes('DEFAULT_PERSIST_NAMESPACE')).toBe(false);
    expect(source.includes('legacyPreviewStateKey')).toBe(false);
    expect(source.includes("localStorage.getItem('nomifun_preview_state')")).toBe(false);
    expect(source.includes('getBrowserStorageGeneration()')).toBe(true);
    expect(source.includes('previewPersistenceNamespace')).toBe(true);
  });

  test('scopes conversation, terminal and transcript providers by stable entity id', () => {
    const chatLayout = readSource(new URL('../../components/ChatLayout/index.tsx', import.meta.url));
    const terminal = readSource(new URL('../../../terminal/TerminalSessionPage.tsx', import.meta.url));
    const transcript = readSource(new URL('../../execution/ReadOnlyConversationView.tsx', import.meta.url));

    expect(chatLayout.includes('persistNamespace={previewScope}')).toBe(true);
    expect(chatLayout.includes('key={previewScope}')).toBe(true);
    expect(chatLayout.includes("props.conversation_id ?? 'pending'")).toBe(false);
    expect(chatLayout.includes('conversation-pending:${uuid()}')).toBe(true);
    expect(terminal.includes('persistNamespace={`terminal:${sessionId}`}')).toBe(true);
    expect(terminal.includes('<TerminalSessionContent key={sessionId} sessionId={sessionId} />')).toBe(true);
    expect(transcript.includes('conversation.execution_attempt_id ?? conversation.execution_step_id')).toBe(true);
    expect(transcript.includes('persistNamespace={`execution-transcript:${transcriptEntityId}`}')).toBe(true);
  });
});
