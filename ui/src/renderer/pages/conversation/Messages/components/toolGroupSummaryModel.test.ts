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
  test('does not classify update_plan as a file edit', () => {
    const parts = buildToolReceiptSummaryParts(
      [tool({ key: 'plan-1', name: 'update_plan', status: 'completed' })],
      'completed'
    );

    expect(parts).toEqual([
      {
        action: 'generic',
        count: 1,
        state: 'completed',
        target: 'update_plan',
      },
    ]);
  });

  test('keeps domain update and search tools generic', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({ key: 'kb-update', name: 'nomi_knowledge_update_base', status: 'error' }),
        tool({ key: 'kb-search', name: 'knowledge_search', status: 'completed' }),
      ],
      'failed'
    );

    expect(parts).toEqual([
      {
        action: 'generic',
        count: 2,
        state: 'failed',
        target: 'nomi_knowledge_update_base, knowledge_search',
      },
    ]);
  });

  test('classifies anchored file actions through direct and MCP names', () => {
    const rows = buildToolReceiptDetailRows([
      tool({ key: 'read-file', name: 'read_file' }),
      tool({ key: 'write-file', name: 'write_file' }),
      tool({ key: 'list-directory', name: 'list_directory' }),
        tool({ key: 'mcp-read-file', name: 'mcp__server__read_file' }),
        tool({ key: 'mcp-write-file', name: 'mcp__server__write_file' }),
        tool({ key: 'mcp-list-directory', name: 'mcp__server__list_directory' }),
        tool({
          key: 'canonical-mcp-read-file',
          name: 'mcp__server__read_file__abcdefghijklmnop',
        }),
    ]);

    expect(rows.map(({ title, action }) => ({ title, action }))).toEqual([
      { title: 'read_file', action: 'read_files' },
      { title: 'write_file', action: 'edit_files' },
      { title: 'list_directory', action: 'list_files' },
      { title: 'server/read_file', action: 'read_files' },
      { title: 'server/write_file', action: 'edit_files' },
      { title: 'server/list_directory', action: 'list_files' },
      {
        title: 'server/read_file',
        action: 'read_files',
      },
    ]);
  });

  test('keeps canonical knowledge aliases generic after stripping routing hash', () => {
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'canonical-kb-update',
        name: 'mcp__gateway__nomi_knowledge_update_base__abcdefghijklmnop',
      }),
    ]);

    expect(rows.map(({ title, action }) => ({ title, action }))).toEqual([
      {
        title: 'gateway/nomi_knowledge_update_base',
        action: 'generic',
      },
    ]);
  });

  test('keeps ambiguous one-word canonical MCP actions generic without an explicit kind', () => {
    const rows = buildToolReceiptDetailRows([
      tool({ key: 'web-search', name: 'mcp__web__search__abcdefghijklmnop' }),
      tool({ key: 'knowledge-read', name: 'mcp__knowledge__read__bcdefghijklmnopq' }),
      tool({ key: 'workflow-run', name: 'mcp__workflow__run__cdefghijklmnopqr' }),
      tool({ key: 'domain-list', name: 'mcp__domain__list__defghijklmnopqrs' }),
    ]);

    expect(rows.map(({ title, action }) => ({ title, action }))).toEqual([
      { title: 'web/search', action: 'generic' },
      { title: 'knowledge/read', action: 'generic' },
      { title: 'workflow/run', action: 'generic' },
      { title: 'domain/list', action: 'generic' },
    ]);
  });

  test('classifies ToolSearch as tool loading', () => {
    const parts = buildToolReceiptSummaryParts(
      [tool({ key: 'tool-search', name: 'ToolSearch', status: 'completed' })],
      'completed'
    );

    expect(parts).toEqual([{ action: 'load_tools', count: 1, state: 'completed' }]);
  });

  test('uses explicit ACP semantics before a conflicting tool-name action', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({
          key: 'acp-read',
          name: 'write_file',
          kind: 'read',
          description: 'config.yaml',
          input: '{"path":"config.yaml"}',
        }),
      ],
      'completed'
    );

    expect(parts).toEqual([
      { action: 'read_files', count: 1, state: 'completed', target: 'config.yaml' },
    ]);
  });

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

  test('summarizes non-fatal command exits as completed command runs', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({
          key: 'grep',
          name: 'Bash',
          description: 'grep -rn "missing" .',
          status: 'error',
          nonFatalFailure: true,
        }),
      ],
      'completed'
    );

    expect(parts).toEqual([
      { action: 'run_commands', count: 1, state: 'completed', target: 'grep -rn "missing" .' },
    ]);
  });

  test('summarizes prior-error barrier commands as skipped instead of failed', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({
          key: 'bash-skipped',
          name: 'Bash',
          status: 'canceled',
          skipped: true,
          input: '{"command":"find /workspace -maxdepth 2 -type d"}',
        }),
      ],
      'canceled'
    );

    expect(parts).toEqual([
      {
        action: 'run_commands',
        count: 1,
        state: 'canceled',
        target: 'find /workspace -maxdepth 2 -type d',
        skipped: true,
      },
    ]);
  });

  test('keeps invalid-argument rejections separate from tools that actually ran', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({
          key: 'delegate-invalid',
          name: 'mcp__nomifun-desktop__nomi_delegate__anxmvqfkcuzfi4mq',
          status: 'canceled',
          notExecutedReason: 'invalid_arguments',
        }),
        tool({
          key: 'delegate-success',
          name: 'mcp__nomifun-desktop__nomi_delegate__anxmvqfkcuzfi4mq',
          status: 'completed',
        }),
      ],
      'completed'
    );

    expect(parts).toEqual([
      {
        action: 'generic',
        count: 1,
        state: 'completed',
        target: 'nomifun-desktop/nomi_delegate',
        notExecutedReason: 'invalid_arguments',
      },
      {
        action: 'generic',
        count: 1,
        state: 'completed',
        target: 'nomifun-desktop/nomi_delegate',
      },
    ]);
  });

  test('handles structured tool descriptions without throwing during receipt rendering', () => {
    const parts = buildToolReceiptSummaryParts(
      [
        tool({
          key: 'structured',
          name: 'Bash',
          description: { command: 'codex --version' } as any,
          status: 'running',
        }),
      ],
      'running'
    );

    expect(parts).toEqual([
      {
        action: 'run_commands',
        count: 1,
        state: 'running',
        target: '{ "command": "codex --version" }',
      },
    ]);
  });

  test('does not reintroduce a raw MCP routing name through description', () => {
    const rawName = 'mcp__nomifun-desktop__nomi_delegate__anxmvqfkcuzfi4mq';
    const parts = buildToolReceiptSummaryParts(
      [
        tool({
          key: 'delegate-invalid',
          name: rawName,
          description: rawName,
          status: 'canceled',
          notExecutedReason: 'invalid_arguments',
        }),
      ],
      'completed'
    );

    expect(parts[0]?.target).toBe('nomifun-desktop/nomi_delegate');
    expect(parts[0]?.target?.includes('anxmvqfkcuzfi4mq')).toBe(false);
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

  test('keeps non-fatal command exit details inspectable without failed row styling', () => {
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'grep',
        name: 'Bash',
        description: 'grep -rn "missing" .',
        status: 'error',
        nonFatalFailure: true,
        output: 'exit code 1',
      }),
    ]);

    expect(rows).toEqual([
      {
        key: 'grep',
        action: 'run_commands',
        state: 'completed',
        title: 'Bash',
        target: 'grep -rn "missing" .',
        output: 'exit code 1',
      },
    ]);
  });

  test('keeps skipped command details distinct from user cancellation', () => {
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'bash-skipped',
        name: 'Bash',
        status: 'canceled',
        skipped: true,
        input: '{"command":"find /workspace -maxdepth 2 -type d"}',
      }),
    ]);

    expect(rows).toEqual([
      {
        key: 'bash-skipped',
        action: 'run_commands',
        state: 'canceled',
        title: 'Bash',
        target: 'find /workspace -maxdepth 2 -type d',
        input: '{"command":"find /workspace -maxdepth 2 -type d"}',
        skipped: true,
      },
    ]);
  });

  test('keeps local validation diagnostics inspectable under a stable MCP title', () => {
    const output =
      "Invalid arguments for tool 'mcp__nomifun-desktop__nomi_delegate__anxmvqfkcuzfi4mq': " +
      'JSON Schema validation failed. Correct the arguments and retry; the tool was not executed.';
    const rows = buildToolReceiptDetailRows([
      tool({
        key: 'delegate-invalid',
        name: 'mcp__nomifun-desktop__nomi_delegate__anxmvqfkcuzfi4mq',
        status: 'canceled',
        notExecutedReason: 'invalid_arguments',
        output,
      }),
    ]);

    expect(rows).toEqual([
      {
        key: 'delegate-invalid',
        action: 'generic',
        state: 'completed',
        title: 'nomifun-desktop/nomi_delegate',
        target: 'nomifun-desktop/nomi_delegate',
        output,
        notExecutedReason: 'invalid_arguments',
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
