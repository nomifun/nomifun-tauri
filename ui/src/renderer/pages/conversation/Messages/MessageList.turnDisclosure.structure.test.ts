/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./MessageList.tsx', import.meta.url), 'utf8');
const buildSummarySource = source.slice(
  source.indexOf('const buildProcessReceiptSummary'),
  source.indexOf('const highlightStyle')
);

describe('MessageList turn completion disclosure structure', () => {
  test('routes message content through the turn disclosure model before rendering', () => {
    expect(source.includes('buildTurnDisclosureItems')).toBe(true);
    expect(source.includes('assignTurnIdsFromUserRequests')).toBe(true);
    expect(source.includes('tailClosed: conversationContext?.isProcessing !== true')).toBe(true);
    expect(source.includes("type: 'turn_process_disclosure'")).toBe(true);
    expect(source.includes('renderTurnDisclosure')).toBe(true);
    expect(source.includes('components/TurnProcessDisclosure')).toBe(true);
    expect(source.includes("type: 'process_receipt'")).toBe(true);
    expect(source.includes('renderProcessReceipt')).toBe(true);
    expect(source.includes('components/TurnProcessReceipt')).toBe(true);
    expect(source.includes('components/ProcessTraceItem')).toBe(true);
    expect(source.includes('renderProcessTraceItem')).toBe(true);
    expect(source.includes('getProcessItemState')).toBe(true);
    expect(source.includes('highlighted={highlighted}')).toBe(true);
  });

  test('does not reuse legacy process cards inside receipt expansion', () => {
    expect(source.includes("renderProcessItem={(processItem) => renderProcessTraceItem(processItem, 'list', workspaceRoots)}")).toBe(true);
    expect(source.includes('MessageToolGroupSummary')).toBe(false);
    expect(source.includes('defaultExpanded={true}')).toBe(false);
  });

  test('keeps completed thinking as process evidence for disclosure duration and audit', () => {
    expect(source.includes("if (message.type === 'thinking' && message.content.status === 'done') continue;")).toBe(false);
    expect(source.includes("if (message.type === 'thinking') continue;")).toBe(false);
    expect(source.includes('thinkingCompletedWithDuration')).toBe(false);
    expect(source.includes('getProcessedItemProcessStartedAt')).toBe(true);
    expect(source.includes('getProcessedItemProcessEndedAt')).toBe(true);
  });

  test('renders readable thinking receipts as solid text instead of an expandable receipt shell', () => {
    expect(source.includes('isReadableThinkingReceipt')).toBe(true);
    expect(source.includes("if (isReadableThinkingReceipt(item)) {")).toBe(true);
    expect(source.includes("renderProcessTraceItem(item.item, 'receipt', workspaceRoots)")).toBe(true);
  });

  test('keeps model activity receipts as static single-line status rows', () => {
    const agentStatusCase = buildSummarySource.match(/case 'agent_status':[\s\S]*?case 'tips':/)?.[0] ?? '';

    expect(agentStatusCase.includes("item.content.status === 'preparing'")).toBe(true);
    expect(agentStatusCase.includes("item.content.status === 'prepared'")).toBe(true);
    expect(agentStatusCase.includes('hasDetail: false')).toBe(true);
  });

  test('marks only genuinely detailed receipts as expandable', () => {
    const toolSummaryCase =
      buildSummarySource.match(/if \('type' in item && item\.type === 'tool_summary'\) \{[\s\S]*?if \('type' in item && item\.type === 'file_summary'\)/)?.[0] ?? '';
    const fileSummaryCase =
      buildSummarySource.match(/if \('type' in item && item\.type === 'file_summary'\) \{[\s\S]*?if \('type' in item && item\.type === 'artifact'\)/)?.[0] ?? '';
    const permissionCase = buildSummarySource.match(/case 'permission':[\s\S]*?case 'agent_status':/)?.[0] ?? '';

    expect(toolSummaryCase.includes('hasDetail: true')).toBe(true);
    expect(fileSummaryCase.includes('hasDetail: item.diffs.length > 1')).toBe(true);
    expect(permissionCase.match(/hasDetail: true/g) ?? []).toHaveLength(2);
  });

  test('routes context compaction tips through process receipts instead of assistant text', () => {
    expect(source.includes('isContextCompressionTip')).toBe(true);
    expect(source.includes("if (isContextCompressionTip(item)) return 'process';")).toBe(true);
  });

  test('keeps the implementation scoped to the message content area', () => {
    expect(source.includes('PreviewPanel')).toBe(false);
    expect(source.includes('OrchestrationTopPanel')).toBe(false);
    expect(source.includes('ProjectedWorkerView')).toBe(false);
  });
});
