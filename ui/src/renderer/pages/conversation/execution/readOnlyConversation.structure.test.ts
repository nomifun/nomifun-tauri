/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('execution transcript capability boundary', () => {
  test('marks every projected platform chat as read-only', () => {
    const source = readSource(new URL('./ReadOnlyConversationView.tsx', import.meta.url));
    const basicPlatformChats = [
      readSource(new URL('../platforms/openclaw/OpenClawChat.tsx', import.meta.url)),
      readSource(new URL('../platforms/nanobot/NanobotChat.tsx', import.meta.url)),
      readSource(new URL('../platforms/remote/RemoteChat.tsx', import.meta.url)),
    ];
    const basicPlatformSendBoxes = [
      readSource(new URL('../platforms/openclaw/OpenClawSendBox.tsx', import.meta.url)),
      readSource(new URL('../platforms/nanobot/NanobotSendBox.tsx', import.meta.url)),
      readSource(new URL('../platforms/remote/RemoteSendBox.tsx', import.meta.url)),
    ];

    expect(source.match(/\breadOnly\b/g)?.length).toBeGreaterThanOrEqual(5);
    expect(source.match(/\bhideSendBox\b/g)?.length).toBeGreaterThanOrEqual(5);
    for (const chatSource of basicPlatformChats) {
      expect(chatSource.includes('readOnly?: boolean')).toBe(true);
      expect(chatSource.includes('useConversationResponseMessages(conversation_id)')).toBe(true);
      expect(chatSource.includes('!readOnly && !hideSendBox')).toBe(true);
    }
    for (const sendBoxSource of basicPlatformSendBoxes) {
      expect(sendBoxSource.includes('transformMessage')).toBe(false);
    }
  });

  test('disables ACP runtime mutations while preserving its stream hook', () => {
    const chatSource = readSource(new URL('../platforms/acp/AcpChat.tsx', import.meta.url));
    const initialMessageSource = readSource(new URL('../platforms/acp/useAcpInitialMessage.ts', import.meta.url));
    const recoverySource = readSource(new URL('../Messages/usePendingConfirmationsRecovery.ts', import.meta.url));

    expect(chatSource.includes('useAcpMessage(conversation_id')).toBe(true);
    expect(chatSource.includes('skipWarmup: readOnly === true')).toBe(true);
    expect(chatSource.includes('enabled: !readOnly')).toBe(true);
    expect(initialMessageSource.includes('if (!enabled) return;')).toBe(true);
    expect(recoverySource.includes('if (!enabled || !conversation_id) return;')).toBe(true);
  });

  test('disables Nomi persistence and local command side effects', () => {
    const chatSource = readSource(new URL('../platforms/nomi/NomiChat.tsx', import.meta.url));
    const messageSource = readSource(new URL('../platforms/nomi/useNomiMessage.ts', import.meta.url));

    expect(chatSource.includes('usePendingConfirmationsRecovery(conversation_id, { enabled: !readOnly })')).toBe(true);
    expect(chatSource.includes('readOnly,')).toBe(true);
    expect(messageSource.includes('if (readOnly || !msgId')).toBe(true);
    expect(messageSource.includes('if (!readOnly) {')).toBe(true);
    expect(messageSource.includes('ipcBridge.conversation.update.invoke')).toBe(true);
  });

  test('removes every confirmation action from transcript rendering', () => {
    const permissionSource = readSource(new URL('../Messages/components/MessagePermission.tsx', import.meta.url));
    const acpPermissionSource = readSource(new URL('../Messages/acp/MessageAcpPermission.tsx', import.meta.url));
    const toolGroupSource = readSource(new URL('../Messages/components/MessageToolGroup.tsx', import.meta.url));

    for (const source of [permissionSource, acpPermissionSource]) {
      expect(source.includes('useConversationContextSafe()?.readOnly === true')).toBe(true);
      expect(source.includes('if (readOnly || hasResponded || !selected) return;')).toBe(true);
      expect(source.includes('!readOnly && !hasResponded')).toBe(true);
    }

    expect(toolGroupSource.includes("!readOnly && content.status === 'Confirming'")).toBe(true);
    expect(toolGroupSource.includes('if (readOnly) return;')).toBe(true);
  });
});
