/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const chatSource = readFileSync(new URL('./NomiChat.tsx', import.meta.url), 'utf8');
const sendBoxSource = readFileSync(new URL('./NomiSendBox.tsx', import.meta.url), 'utf8');

describe('NomiChat turn activity ownership', () => {
  test('shares the local stream lifecycle with the message list and send box', () => {
    expect(chatSource.includes('useNomiMessage(conversation_id')).toBe(true);
    expect(chatSource.includes('turnActivity.running')).toBe(true);
    expect(chatSource.includes('turnActivity.hasHydratedRunningState')).toBe(true);
    expect(chatSource.includes('isProcessing: resolvedIsProcessing')).toBe(true);
    expect(chatSource.includes('turnActivity={turnActivity}')).toBe(true);
  });

  test('uses the initial processing snapshot only until live turn state is hydrated', () => {
    expect(
      /const resolvedIsProcessing = turnActivity\.hasHydratedRunningState\s+\? turnActivity\.running\s+: isProcessing === true \|\| turnActivity\.running;/.test(
        chatSource
      )
    ).toBe(true);
  });

  test('does not let the send box own the stream subscription by itself', () => {
    expect(sendBoxSource.includes('useNomiMessage(')).toBe(false);
    expect(sendBoxSource.includes('turnActivity: NomiMessageRuntime')).toBe(true);
  });
});
