/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('webhook wire ID contract', () => {
  test('uses webhook_id throughout the client adapter and rejects generic id consumers', () => {
    expect(source.includes('webhook_id: WebhookId;')).toBe(true);
    expect(source.includes('webhook_id: parseWebhookId(value.webhook_id)')).toBe(true);
    expect(source.includes('{ webhook_id: WebhookId }')).toBe(true);
    expect(source.includes('/api/webhooks/${p.webhook_id}')).toBe(true);
    expect(source.includes('httpGet<IWebhook, { id: WebhookId }')).toBe(false);
    expect(source.includes('httpPut<IWebhook, { id: WebhookId')).toBe(false);
    expect(source.includes('httpDelete<void, { id: WebhookId }')).toBe(false);
    expect(source.includes('httpPost<void, { id: WebhookId }')).toBe(false);
    expect(source.includes('parseWebhookId(value.id)')).toBe(false);
  });

  test('parses webhook ids through the strict UUIDv7 business-id boundary', () => {
    expect(source.includes('webhook_id: parseWebhookId(value.webhook_id)')).toBe(true);
    expect(source.includes('positive local webhook row ID')).toBe(false);
  });
});
