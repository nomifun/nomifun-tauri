/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import type { ToolReceiptDetailRow } from './components/toolGroupSummaryModel';
import {
  buildThinkingReceiptDisplay,
  shouldShowFileListDetail,
  shouldShowThinkingReceiptDetail,
  shouldShowToolRowDetail,
} from './processTraceDisplayModel';

const row = (item: Partial<ToolReceiptDetailRow> & Pick<ToolReceiptDetailRow, 'action'>): ToolReceiptDetailRow => ({
  key: item.key ?? 'tool-1',
  state: item.state ?? 'completed',
  title: item.title ?? 'Write',
  ...item,
});

describe('process trace display model', () => {
  test('does not expand a single file receipt when the file name is already visible in the row', () => {
    const writeRow = row({ action: 'edit_files', target: 'snake.html' });

    expect(shouldShowToolRowDetail(writeRow, { fileRowCount: 1 })).toBe(false);
    expect(shouldShowFileListDetail([writeRow])).toBe(false);
  });

  test('does not expand a completed single file row just because the tool echoed output', () => {
    const writeRow = row({ action: 'edit_files', target: 'snake.html', output: 'snake.html' });

    expect(shouldShowToolRowDetail(writeRow, { fileRowCount: 1 })).toBe(false);
  });

  test('keeps failed single file rows expandable so the error remains inspectable', () => {
    const writeRow = row({
      action: 'edit_files',
      state: 'failed',
      target: 'snake.html',
      output: 'Permission denied',
    });

    expect(shouldShowToolRowDetail(writeRow, { fileRowCount: 1 })).toBe(true);
  });

  test('keeps multi-file receipts expandable so the file list remains inspectable', () => {
    const rows = [
      row({ key: 'read-1', action: 'read_files', target: 'MessageList.tsx' }),
      row({ key: 'read-2', action: 'read_files', target: 'ProcessTraceItem.tsx' }),
    ];

    expect(shouldShowFileListDetail(rows)).toBe(true);
    expect(shouldShowToolRowDetail(rows[0], { fileRowCount: rows.length })).toBe(true);
  });

  test('keeps command rows expandable for command input and output', () => {
    expect(shouldShowToolRowDetail(row({ action: 'run_commands', target: 'bun run check' }))).toBe(true);
  });

  test('keeps blank running thinking as a non-expandable waiting receipt', () => {
    const display = buildThinkingReceiptDisplay(
      { content: '', status: 'thinking' },
      {
        runningFallback: 'Thinking',
        waitingFallback: 'Waiting for model output',
      }
    );

    expect(display.label).toBe('Waiting for model output');
    expect(display.detail).toBe('');
    expect(shouldShowThinkingReceiptDetail({ content: '', status: 'thinking' })).toBe(false);
  });

  test('uses streamed thinking content as the receipt preview and expandable detail', () => {
    const display = buildThinkingReceiptDisplay(
      {
        content:
          'I am checking the workspace state and waiting for the first tool call before editing the message list.',
        status: 'thinking',
      },
      {
        runningFallback: 'Thinking',
        waitingFallback: 'Waiting for model output',
      }
    );

    expect(display.label).toBe('I am checking the workspace state and waiting for the first tool call before editing...');
    expect(display.detail.includes('checking the workspace state')).toBe(true);
    expect(shouldShowThinkingReceiptDetail({ content: display.detail, status: 'thinking' })).toBe(true);
  });

  test('keeps completed blank thinking visible through a duration summary', () => {
    const display = buildThinkingReceiptDisplay(
      {
        content: '',
        duration: 34600,
        status: 'done',
      },
      {
        completedFallback: 'Thought 35s',
        runningFallback: 'Thinking',
        waitingFallback: 'Waiting for model output',
      }
    );

    expect(display.label).toBe('Thought 35s');
    expect(display.detail).toBe('');
  });

});
