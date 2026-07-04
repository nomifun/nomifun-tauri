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

  test('keeps completed thinking as Codex-style process evidence, not legacy standalone thinking', () => {
    expect(source.includes('thinkingCompletedWithDuration')).toBe(false);
    expect(source.includes('formatProcessDuration')).toBe(false);
    expect(source.includes("if (item.content.status === 'done') return null;")).toBe(false);
    expect(source.includes('ThinkingTraceRow')).toBe(true);
    expect(source.includes('messages.processReceipt.thinkingCompletedDuration')).toBe(true);
  });

  test('renders readable thinking as solid process text without a toggle row', () => {
    expect(source.includes('const thinkingClassName = classNames(')).toBe(true);
    expect(source.includes("variant === 'receipt' && 'turn-process-trace-receipt-detail'")).toBe(true);
    expect(source.includes("variant !== 'receipt' && 'turn-process-trace'")).toBe(true);
    expect(source.includes("variant !== 'receipt' && 'turn-process-trace-thinking-inline'")).toBe(true);
  });

  test('does not use default-expanded thinking toggles for readable thinking text', () => {
    expect(source.includes('defaultExpanded={shouldShowThinkingReceiptDetail(item.content)}')).toBe(false);
    expect(source.includes('defaultExpanded?: boolean')).toBe(false);
  });

  test('renders running thinking and context compression as lightweight process rows', () => {
    expect(source.includes('messages.processReceipt.thinkingRunning')).toBe(true);
    expect(source.includes('messages.processReceipt.thinkingWaiting')).toBe(true);
    expect(source.includes('messages.processReceipt.contextCompressed')).toBe(true);
  });

  test('renders read and edit steps with expandable file lists', () => {
    expect(source.includes('ToolFileListDetail')).toBe(true);
    expect(source.includes('ToolFileGroupTraceRow')).toBe(true);
    expect(source.includes('showLabel = true')).toBe(true);
    expect(source.includes('showLabel={false}')).toBe(true);
    expect(source.includes('isFileReceiptRow')).toBe(true);
    expect(source.includes('shouldShowFileListDetail')).toBe(true);
    expect(source.includes('shouldShowToolRowDetail')).toBe(true);
    expect(source.includes('turn-process-trace-file-list')).toBe(true);
    expect(source.includes('messages.processReceipt.readTargets')).toBe(true);
    expect(source.includes('messages.processReceipt.fileEditTargets')).toBe(true);
  });
});
