/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./ipcBridge.ts', import.meta.url), 'utf8');

describe('requirement attachment wire contract', () => {
  test('maps attachment_id to the UI id without accepting a generic wire id', () => {
    expect(source.includes('export interface AttachmentResponse')).toBe(true);
    expect(source.includes('attachment_id: string;')).toBe(true);
    expect(source.includes('const { attachment_id, ...fields } = attachment')).toBe(true);
    expect(source.includes('id: parseAttachmentId(attachment_id)')).toBe(true);
    expect(source.includes('attachments: requirement.attachments.map(fromAttachmentResponse)')).toBe(true);
    expect(source.includes('parseAttachmentId(attachment.id)')).toBe(false);
  });
});
