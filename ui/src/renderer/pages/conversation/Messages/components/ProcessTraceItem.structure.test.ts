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

  test('renders thinking as neutral process content instead of legacy receipts', () => {
    expect(source.includes('ThinkingStreamPanel')).toBe(false);
    expect(source.includes('useStreamingThinkingText')).toBe(false);
    expect(source.includes('shouldAutoCollapseThinkingStreamPanel')).toBe(false);
    expect(source.includes('turn-process-thinking-stream')).toBe(false);
    expect(source.includes("case 'thinking':")).toBe(true);
    expect(source.includes('<MessageThinking')).toBe(true);
    expect(source.includes('message={item}')).toBe(true);
    expect(source.includes("variant='process'")).toBe(true);
    expect(source.includes('expanded={thinkingExpansion?.expanded}')).toBe(true);
    expect(source.includes('ThinkingTraceRow')).toBe(false);
    expect(source.includes('messages.processReceipt.thinkingCompletedDuration')).toBe(false);
    expect(source.includes('messages.processReceipt.thinkingRunning')).toBe(false);
    expect(source.includes('messages.processReceipt.thinkingWaiting')).toBe(false);
    expect(source.includes('turn-process-trace-thinking-inline')).toBe(false);
    expect(source.includes('defaultExpanded={shouldShowThinkingReceiptDetail(item.content)}')).toBe(false);
  });

  test('renders context compression as lightweight process rows', () => {
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

  test('can render closed process details with an effective state override', () => {
    expect(source.includes('stateOverride?: TurnDisclosureProcessState')).toBe(true);
    expect(source.includes('const state = stateOverride ?? getProcessItemState(item);')).toBe(true);
    expect(source.includes('stateOverride={stateOverride}')).toBe(true);
  });
});
