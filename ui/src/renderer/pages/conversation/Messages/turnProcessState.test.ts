/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { getProcessItemState, getToolMessagesProcessState } from './turnProcessState';

describe('turn process state', () => {
  test('treats tool confirmations as waiting for user input', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_group',
          content: [{ call_id: 'call-1', name: 'Edit', description: '', render_output_as_markdown: false, status: 'Confirming' }],
        } as any,
      ])
    ).toBe('waiting');
  });

  test('surfaces failed and canceled tool states', () => {
    expect(
      getToolMessagesProcessState([
        { type: 'tool_call', content: { call_id: 'call-1', name: 'Bash', args: {}, status: 'error' } } as any,
      ])
    ).toBe('failed');
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_group',
          content: [{ call_id: 'call-1', name: 'Edit', description: '', render_output_as_markdown: false, status: 'Canceled' }],
        } as any,
      ])
    ).toBe('canceled');
  });

  test('keeps the root error failed while classifying barrier-skipped commands as canceled', () => {
    const skipped = {
      type: 'tool_call',
      content: {
        call_id: 'call-bash',
        name: 'Bash',
        status: 'error',
        args: { command: 'find /workspace -maxdepth 2 -type d' },
        output:
          'Skipped because a previous tool call in this assistant turn failed. Inspect the failed result first.',
      },
    } as any;
    const failedKnowledgeRead = {
      type: 'tool_call',
      content: {
        call_id: 'call-knowledge',
        name: 'knowledge_read',
        status: 'error',
        args: { handle: '/workspace/overview.md' },
        output: 'knowledge_read failed: invalid handle: /workspace/overview.md',
      },
    } as any;

    expect(getToolMessagesProcessState([skipped])).toBe('canceled');
    expect(getToolMessagesProcessState([failedKnowledgeRead, skipped])).toBe('failed');
  });

  test('maps local invalid-argument rejection to not executed while keeping real failures failed', () => {
    const name = 'mcp__nomifun-desktop__nomi_delegate__anxmvqfkcuzfi4mq';
    const rejected = {
      type: 'tool_call',
      content: {
        call_id: 'call-invalid',
        name,
        status: 'error',
        args: null,
        output:
          `Invalid arguments for tool '${name}': JSON Schema validation failed: bad value. ` +
          'Correct the arguments and retry; the tool was not executed.',
      },
    } as any;
    const remoteFailure = {
      type: 'tool_call',
      content: {
        call_id: 'call-remote',
        name,
        status: 'error',
        args: null,
        output: 'Remote tool failed after dispatch',
      },
    } as any;

    expect(getToolMessagesProcessState([rejected])).toBe('completed');
    expect(getToolMessagesProcessState([remoteFailure])).toBe('failed');
    expect(getToolMessagesProcessState([rejected, remoteFailure])).toBe('failed');
  });

  test('does not let non-fatal ACP shell command exits fail the whole process receipt', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'acp_tool_call',
          content: {
            update: {
              sessionUpdate: 'tool_call_update',
              tool_call_id: 'call-bash',
              title: 'Bash',
              kind: 'execute',
              status: 'failed',
              rawInput: { command: 'grep -rn "missing" .' },
            },
          },
        } as any,
      ])
    ).toBe('completed');
  });

  test('does not let an ordinary Nomi Bash exit fail the whole process receipt', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_call',
          content: {
            call_id: 'call-bash',
            name: 'Bash',
            status: 'error',
            args: { command: 'node test.js' },
            output: 'Exit code: 1\nSTDERR:\nReferenceError: location is not defined',
          },
        } as any,
      ])
    ).toBe('completed');
  });

  test('keeps Nomi Bash timeouts as failed process evidence', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_call',
          content: {
            call_id: 'call-bash',
            name: 'Bash',
            status: 'error',
            args: { command: 'node test.js' },
            output: 'Command timed out after 120000ms.\nPartial output:\nRESULT_PASS',
          },
        } as any,
      ])
    ).toBe('failed');
  });

  test('does not let failed ACP read probes fail the whole process receipt', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'acp_tool_call',
          content: {
            update: {
              sessionUpdate: 'tool_call_update',
              tool_call_id: 'call-read',
              title: 'config.yaml',
              kind: 'read',
              status: 'failed',
              rawInput: { path: 'config.yaml' },
              content: [
                {
                  type: 'content',
                  content: { type: 'text', text: 'No such file or directory (os error 2)' },
                },
              ],
            },
          },
        } as any,
      ])
    ).toBe('completed');
  });

  test('does not let an ordinary Nomi read miss fail the whole process receipt', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_call',
          content: {
            call_id: 'call-read',
            name: 'Read',
            status: 'error',
            args: { file_path: 'missing.file' },
            output: 'Failed to read file missing.file: No such file or directory (os error 2)',
          },
        } as any,
      ])
    ).toBe('completed');
  });

  test('keeps Nomi read permission failures as failed process evidence', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_call',
          content: {
            call_id: 'call-read',
            name: 'Read',
            status: 'error',
            args: { file_path: 'secret.file' },
            output: 'Failed to read file secret.file: Permission denied (os error 13)',
          },
        } as any,
      ])
    ).toBe('failed');
  });

  test('does not let a failed confirmed shell command group fail the whole process receipt', () => {
    expect(
      getToolMessagesProcessState([
        {
          type: 'tool_group',
          content: [
            {
              call_id: 'call-shell',
              name: 'Bash',
              status: 'Error',
              description: 'Run a validation command',
              confirmationDetails: {
                type: 'exec',
                title: 'Run command',
                command: 'node test.js',
              },
            },
          ],
        } as any,
      ])
    ).toBe('completed');
  });

  test('keeps permission and active thinking steps open', () => {
    expect(getProcessItemState({ type: 'permission' } as any)).toBe('waiting');
    expect(getProcessItemState({ type: 'thinking', content: { status: 'thinking' } } as any)).toBe('running');
  });

  test('marks error tips and agent errors as failed process evidence', () => {
    expect(getProcessItemState({ type: 'tips', content: { type: 'error' } } as any)).toBe('failed');
    expect(getProcessItemState({ type: 'agent_status', content: { status: 'error' } } as any)).toBe('failed');
  });

  test('keeps preparing agent status as a running process step', () => {
    expect(getProcessItemState({ type: 'agent_status', content: { status: 'preparing' } } as any)).toBe('running');
    expect(getProcessItemState({ type: 'agent_status', content: { status: 'prepared' } } as any)).toBe('completed');
  });
});
