/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./ProcessTraceItem.tsx', import.meta.url), 'utf8');

describe('ProcessTraceItem Codex-style execution rows', () => {
  test('keeps tool rows interactive with expandable detail panels', () => {
    expect(source.includes('ToolTraceRow')).toBe(true);
    expect(source.includes('aria-expanded={expanded}')).toBe(true);
    expect(source.includes('turn-process-trace-detail')).toBe(true);
    expect(source.includes('messages.toolDetailInput')).toBe(true);
    expect(source.includes('messages.toolDetailOutput')).toBe(true);
  });

  test('does not render completed thinking duration as a process row', () => {
    expect(source.includes('thinkingCompletedWithDuration')).toBe(false);
    expect(source.includes('formatProcessDuration')).toBe(false);
  });

  test('renders running thinking and context compression as lightweight process rows', () => {
    expect(source.includes("item.content.status === 'done'")).toBe(true);
    expect(source.includes('messages.processReceipt.thinkingRunning')).toBe(true);
    expect(source.includes('messages.processReceipt.contextCompressed')).toBe(true);
  });
});
