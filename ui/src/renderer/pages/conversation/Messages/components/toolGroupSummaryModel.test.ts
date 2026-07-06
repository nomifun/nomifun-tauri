/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { NormalizedToolCall } from '@/common/chat/normalizeToolCall';
import { describe, expect, test } from 'bun:test';
import {
  buildToolReceiptDetailRows,
  buildToolReceiptSummaryParts,
  buildToolSummaryDescriptor,
  getToolReceiptIconFromSummaryParts,
} from './toolGroupSummaryModel';

const tool = (item: Partial<NormalizedToolCall> & Pick<NormalizedToolCall, 'key' | 'name'>): NormalizedToolCall => ({
  status: 'completed',
  ...item,
});

describe('buildToolReceiptSummaryParts', () => {
  test('summarizes mixed file reads and commands as separate receipt parts', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({ key: 'read-1', name: 'Read', description: 'MessageList.tsx' }),
        tool({ key: 'read-2', name: 'Read', description: 'messages.css' }),
        tool({ key: 'read-3', name: 'Read', description: 'turnDisclosureModel.ts' }),
        tool({ key: 'read-4', name: 'Read', description: 'toolGroupSummaryModel.ts' }),
        tool({ key: 'test', name: 'Bash', description: 'bun test ui/src/renderer/pages/conversation/Messages' }),
      ],
      'completed'
    );

    expect(parts).toEqual([
      {
        action: 'read_files',
        count: 4,
        state: 'completed',
        target: 'MessageList.tsx, messages.css, turnDisclosureModel.ts, toolGroupSummaryModel.ts',
      },
      {
        action: 'run_commands',
        count: 1,
        state: 'completed',
        target: 'bun test ui/src/renderer/pages/conversation/Messages',
      },
    ]);
  });

  test('keeps the command title preview for a single running command', () => {
    const parts = buildToolReceiptSummaryParts(
      [tool({ key: 'test', name: 'Bash', description: 'bun test turnDisclosureModel.test.ts', status: 'running' })],
      'running'
    );

    expect(parts).toEqual([
      { action: 'run_commands', count: 1, state: 'running', target: 'bun test turnDisclosureModel.test.ts' },
    ]);
  });

  test('uses the concrete command from input when a shell tool has no description', () => {
    const parts = buildToolReceiptSummaryParts(
      [tool({ key: 'check', name: 'Bash', input: '{"command":"bun run check"}', status: 'running' })],
      'running'
    );

    expect(parts).toEqual([
      { action: 'run_commands', count: 1, state: 'running', target: 'bun run check' },
    ]);
  });

  test('uses the concrete file target from a running write input preview', () => {
    const parts = buildToolReceiptSummaryParts(
      [tool({ key: 'write', name: 'Write', input: '{"file_path":"/tmp/snake.html"}', status: 'running' })],
      'running'
    );

    expect(parts).toEqual([{ action: 'edit_files', count: 1, state: 'running', target: '/tmp/snake.html' }]);
  });

  test('recognizes code search and file listing as scan-friendly receipt titles', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({ key: 'rg', name: 'Grep', description: 'turnDisclosure' }),
        tool({ key: 'list', name: 'Glob', description: 'ui/src/**/*.tsx' }),
      ],
      'completed'
    );

    expect(parts).toEqual([
      { action: 'search_code', count: 1, state: 'completed' },
      { action: 'list_files', count: 1, state: 'completed' },
    ]);
  });

  test('keeps completed read status separate from a running command in the same receipt', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({ key: 'read', name: 'Read', description: 'MessageList.tsx', status: 'completed' }),
        tool({ key: 'test', name: 'Bash', description: 'bun test MessageList', status: 'running' }),
      ],
      'running'
    );

    expect(parts).toEqual([
      { action: 'read_files', count: 1, state: 'completed', target: 'MessageList.tsx' },
      { action: 'run_commands', count: 1, state: 'running', target: 'bun test MessageList' },
    ]);
  });
});

describe('getToolReceiptIconFromSummaryParts', () => {
  test('maps file-list summaries to the file receipt icon', () => {
    const parts = buildToolReceiptSummaryParts([tool({ key: 'list', name: 'Glob', description: 'ui/src/**/*.tsx' })], 'completed');

    expect(getToolReceiptIconFromSummaryParts(parts)).toBe('file');
  });

  test('maps command and edit summaries to distinct Codex-style receipt icons', () => {
    expect(
      getToolReceiptIconFromSummaryParts(
        buildToolReceiptSummaryParts([tool({ key: 'run', name: 'Bash', description: 'dir' })], 'completed')
      )
    ).toBe('tool');
    expect(
      getToolReceiptIconFromSummaryParts(
        buildToolReceiptSummaryParts([tool({ key: 'write', name: 'Write', input: '{"file_path":"a.ts"}' })], 'completed')
      )
    ).toBe('edit');
  });
});

describe('buildToolSummaryDescriptor', () => {
  test('focuses the active tool before older completed tools', () => {
    const descriptor = buildToolSummaryDescriptor(
      [
        tool({ key: 'read', name: 'Read', description: 'messages.css', status: 'completed' }),
        tool({ key: 'test', name: 'Bash', description: 'bun test ...', status: 'running' }),
      ],
      'running'
    );

    expect(descriptor?.target).toBe('bun test ...');
    expect(descriptor?.count).toBe(2);
  });

  test('focuses failed tools when the group failed', () => {
    const descriptor = buildToolSummaryDescriptor(
      [
        tool({ key: 'read', name: 'Read', description: 'messages.css', status: 'completed' }),
        tool({ key: 'test', name: 'Bash', description: 'bun test ...', status: 'error' }),
      ],
      'failed'
    );

    expect(descriptor?.target).toBe('bun test ...');
  });

  test('uses the latest completed tool for completed groups', () => {
    const descriptor = buildToolSummaryDescriptor(
      [
        tool({ key: 'read', name: 'Read', description: 'messages.css' }),
        tool({ key: 'edit', name: 'Edit', description: 'MessageList.tsx' }),
      ],
      'completed'
    );

    expect(descriptor?.target).toBe('Edit MessageList.tsx');
  });
});

describe('buildToolReceiptDetailRows', () => {
  test('keeps individual read and command steps as compact receipt rows', () => {
    const rows = buildToolReceiptDetailRows([
      tool({ key: 'read-1', name: 'Read', description: 'turnDisclosureModel.ts' }),
      tool({ key: 'read-2', name: 'Read', description: 'MessageList.tsx' }),
      tool({ key: 'status', name: 'Bash', description: 'git status --short --branch' }),
    ]);

    expect(rows).toEqual([
      {
        key: 'read-1',
        action: 'read_files',
        state: 'completed',
        title: 'Read',
        target: 'turnDisclosureModel.ts',
      },
      {
        key: 'read-2',
        action: 'read_files',
        state: 'completed',
        title: 'Read',
        target: 'MessageList.tsx',
      },
      {
        key: 'status',
        action: 'run_commands',
        state: 'completed',
        title: 'Bash',
        target: 'git status --short --branch',
      },
    ]);
  });

  test('preserves running state for active tools in receipt details', () => {
    const rows = buildToolReceiptDetailRows([
      tool({ key: 'test', name: 'Bash', description: 'bun test MessageList', status: 'running' }),
    ]);

    expect(rows).toEqual([
      {
        key: 'test',
        action: 'run_commands',
        state: 'running',
        title: 'Bash',
        target: 'bun test MessageList',
      },
    ]);
  });

  test('keeps command input and output available for expandable detail panels', () => {
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'test',
        name: 'Bash',
        description: 'python snake.py',
        input: 'python snake.py',
        output: 'pygame 2.6.1\\nGame started',
        truncated: true,
      }),
    ]);

    expect(rows).toEqual([
      {
        key: 'test',
        action: 'run_commands',
        state: 'completed',
        title: 'Bash',
        target: 'python snake.py',
        input: 'python snake.py',
        output: 'pygame 2.6.1\\nGame started',
        truncated: true,
      },
    ]);
  });

  test('extracts read targets from structured input for expandable file lists', () => {
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'read-input',
        name: 'Read',
        input: '{"file_path":"ui/src/renderer/pages/conversation/Messages/MessageList.tsx"}',
      }),
    ]);

    expect(rows).toEqual([
      {
        key: 'read-input',
        action: 'read_files',
        state: 'completed',
        title: 'Read',
        target: 'ui/src/renderer/pages/conversation/Messages/MessageList.tsx',
        input: '{"file_path":"ui/src/renderer/pages/conversation/Messages/MessageList.tsx"}',
      },
    ]);
  });

  test('extracts running write targets from structured input previews', () => {
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'write-input',
        name: 'Write',
        status: 'running',
        input: '{"file_path":"/tmp/snake.html"}',
      }),
    ]);

    expect(rows).toEqual([
      {
        key: 'write-input',
        action: 'edit_files',
        state: 'running',
        title: 'Write',
        target: '/tmp/snake.html',
        input: '{"file_path":"/tmp/snake.html"}',
      },
    ]);
  });
});
