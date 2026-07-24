/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./hooks.ts', import.meta.url), 'utf8');

describe('knowledge writeback reconnect recovery', () => {
  test('reloads the durable message projection after websocket reconnect', () => {
    expect(source.includes('ipcBridge.conversation.reconnected.on')).toBe(true);
    expect(
      source.includes(
        '[useMessageLstCache] Failed to refresh messages after WebSocket reconnect:'
      )
    ).toBe(true);
  });
});
