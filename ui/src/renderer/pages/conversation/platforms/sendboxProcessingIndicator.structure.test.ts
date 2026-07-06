/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const sendBoxFiles = [
  './acp/AcpSendBox.tsx',
  './nomi/NomiSendBox.tsx',
  './remote/RemoteSendBox.tsx',
  './nanobot/NanobotSendBox.tsx',
  './openclaw/OpenClawSendBox.tsx',
];

describe('conversation sendbox processing indicator', () => {
  test('does not render the legacy bottom ThoughtDisplay processing bar', () => {
    for (const filePath of sendBoxFiles) {
      const source = readFileSync(new URL(filePath, import.meta.url), 'utf8');
      expect(source.includes('<ThoughtDisplay')).toBe(false);
    }
  });

  test('removes the obsolete ThoughtDisplay component source', () => {
    expect(existsSync(new URL('../../../components/chat/ThoughtDisplay.tsx', import.meta.url))).toBe(false);
  });
});
