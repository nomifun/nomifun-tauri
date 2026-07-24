/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL): string => readFileSync(url, 'utf8');

describe('Guid initial-message idempotency', () => {
  test('persists one UUIDv7 key in every initial-message payload before navigation', () => {
    const source = readSource(new URL('./hooks/useGuidSend.ts', import.meta.url));

    expect(source.includes("import { uuidv7 } from '@/common/utils';")).toBe(true);
    expect(source.match(/idempotency_key: uuidv7\(\),/g)).toHaveLength(4);
    expect(source.match(/conversation_id: conversation\.id,/g)).toHaveLength(4);
    expect(source.match(/initial_admission_epoch: 0,/g)).toHaveLength(4);

    const storageWrites = [
      "'initial-message-openclaw'",
      "'initial-message-nanobot'",
      "'initial-message-nomi'",
      "'initial-message-remote' : 'initial-message-acp'",
    ];
    for (const marker of storageWrites) {
      expect(source.includes(marker)).toBe(true);
    }

    const writesBeforeNavigation =
      source.lastIndexOf('sessionStorage.setItem') < source.lastIndexOf('await navigate(');
    expect(writesBeforeNavigation).toBe(true);
  });

  test('Nomi QuickStart persists the auto-send key before navigation', () => {
    const source = readSource(
      new URL('../../hooks/agent/useNomiQuickStart.ts', import.meta.url)
    );
    const key = source.indexOf('idempotency_key: uuidv7()');
    const owner = source.indexOf('conversation_id: conversation.id');
    const epoch = source.indexOf('initial_admission_epoch: 0', owner);
    const storageWrite = source.indexOf('sessionStorage.setItem(');
    const navigation = source.indexOf('await navigate(');

    expect(
      source.includes("import { uuidv7 } from '@/common/utils/uuidv7';")
    ).toBe(true);
    expect(storageWrite >= 0).toBe(true);
    expect(owner > storageWrite).toBe(true);
    expect(epoch > owner).toBe(true);
    expect(key > storageWrite).toBe(true);
    expect(navigation > key).toBe(true);
  });
});
