import { describe, expect, it } from 'vitest';
import { normalizeAcpToolCall, normalizeToolCall, normalizeToolGroup } from './normalizeToolCall';

describe('normalizeToolCall', () => {
  it('ignores tool_call messages without call_id', () => {
    const result = normalizeToolCall({
      type: 'tool_call',
      content: {
        call_id: '',
        name: 'Glob',
        status: 'running',
        args: { pattern: '*.rs' },
      },
    } as any);

    expect(result).toBeUndefined();
  });

  it('marks ordinary non-zero Bash exits as non-fatal process outcomes', () => {
    const result = normalizeToolCall({
      type: 'tool_call',
      content: {
        call_id: 'call-bash',
        name: 'Bash',
        status: 'error',
        args: { command: 'node test.js' },
        output: 'Exit code: 1\nSTDERR:\nTypeError: missing browser stub',
      },
    } as any);

    expect(result?.status).toBe('error');
    expect(result?.nonFatalFailure).toBe(true);
  });

  it('marks prior-error barrier results as skipped cancellations', () => {
    const result = normalizeToolCall({
      type: 'tool_call',
      content: {
        call_id: 'call-skipped',
        name: 'Bash',
        status: 'error',
        args: { command: 'find /workspace -maxdepth 2 -type d' },
        output:
          'Skipped because a previous tool call in this assistant turn failed. Inspect the failed result first.',
      },
    } as any);

    expect(result?.status).toBe('canceled');
    expect(result?.skipped).toBe(true);
    expect(result?.nonFatalFailure).toBeUndefined();
  });

  const infrastructureFailures = [
    'Command timed out after 120000ms.\nPartial output:\nRESULT_PASS',
    'Command was cancelled.\nSTDOUT:\npartial',
    'Failed to execute command: executable not found (spawn_failed)',
    'Command cleanup is unproven (pid=42, state=Lost). Do not blindly retry.',
    'The turn ended before this tool completed: channel_closed',
    'Exit code: -1\nSignal: 9\nOUTPUT:',
    'Exit code: 1\nOUTPUT:\nCleanup diagnostics: exact cleanup was not proven',
  ];

  for (const [index, output] of infrastructureFailures.entries()) {
    it(`keeps Bash infrastructure failure ${index + 1} fatal`, () => {
      const result = normalizeToolCall({
        type: 'tool_call',
        content: {
          call_id: 'call-bash',
          name: 'Bash',
          status: 'error',
          args: { command: 'node test.js' },
          output,
        },
      } as any);

      expect(result?.nonFatalFailure).toBeUndefined();
    });
  }

  it('marks only explicit direct read/search probe misses as non-fatal', () => {
    for (const [name, output] of [
      ['Read', 'Failed to read file missing.file: No such file or directory (os error 2)'],
      ['Glob', 'No files matched the pattern'],
      ['Grep', 'No matches found'],
    ]) {
      const result = normalizeToolCall({
        type: 'tool_call',
        content: {
          call_id: `call-${name}`,
          name,
          status: 'error',
          args: { path: 'missing.file' },
          output,
        },
      } as any);

      expect(result?.nonFatalFailure).toBe(true);
    }
  });

  it('keeps direct probe permission and syntax failures fatal', () => {
    for (const [name, output] of [
      ['Read', 'Failed to read file secret.txt: Permission denied (os error 13)'],
      ['Glob', 'Invalid glob pattern: Pattern syntax error near position 2'],
      ['Grep', 'rg error: permission denied'],
    ]) {
      const result = normalizeToolCall({
        type: 'tool_call',
        content: {
          call_id: `call-${name}`,
          name,
          status: 'error',
          args: { path: 'secret.txt' },
          output,
        },
      } as any);

      expect(result?.nonFatalFailure).toBeUndefined();
    }
  });

  it('keeps interrupted direct probes fatal', () => {
    const result = normalizeToolCall({
      type: 'tool_call',
      content: {
        call_id: 'call-read',
        name: 'Read',
        status: 'error',
        args: { path: 'config.json' },
        output: 'The turn ended before this tool completed: channel_closed',
      },
    } as any);

    expect(result?.nonFatalFailure).toBeUndefined();
  });
});

describe('normalizeToolGroup', () => {
  it('marks failed confirmed shell commands as non-fatal process outcomes', () => {
    const [result] = normalizeToolGroup({
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
    } as any);

    expect(result.status).toBe('error');
    expect(result.nonFatalFailure).toBe(true);
  });

  it('keeps failed edit groups fatal', () => {
    const [result] = normalizeToolGroup({
      type: 'tool_group',
      content: [
        {
          call_id: 'call-edit',
          name: 'Edit',
          status: 'Error',
          confirmationDetails: {
            type: 'edit',
            title: 'Apply edit',
            file_name: 'app.ts',
            file_diff: '',
          },
        },
      ],
    } as any);

    expect(result.nonFatalFailure).toBeUndefined();
  });
});

describe('normalizeAcpToolCall', () => {
  it('marks failed ACP shell commands as non-fatal process outcomes', () => {
    const result = normalizeAcpToolCall({
      type: 'acp_tool_call',
      id: 'msg-1',
      conversation_id: 1,
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'tool-1',
          title: 'Bash',
          kind: 'execute',
          status: 'failed',
          rawInput: {
            command: 'grep -rn "needle" .',
          },
        },
      },
    } as any);

    expect(result?.status).toBe('error');
    expect(result?.nonFatalFailure).toBe(true);
  });

  it('extracts nested ACP execute commands without leaking structured values into descriptions', () => {
    const result = normalizeAcpToolCall({
      type: 'acp_tool_call',
      id: 'msg-1',
      conversation_id: 1,
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'tool-1',
          title: 'Bash',
          kind: 'execute',
          status: 'in_progress',
          rawInput: {
            command: {
              cmd: 'codex --version',
            },
          },
        },
      },
    } as any);

    expect(result?.description).toBe('codex --version');
  });

  it('marks explicit ACP read misses as non-fatal process outcomes', () => {
    const result = normalizeAcpToolCall({
      type: 'acp_tool_call',
      id: 'msg-1',
      conversation_id: 1,
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'tool-1',
          title: 'config.yaml',
          kind: 'read',
          status: 'failed',
          rawInput: {
            path: 'config.yaml',
          },
          content: [
            {
              type: 'content',
              content: { type: 'text', text: 'No such file or directory (os error 2)' },
            },
          ],
        },
      },
    } as any);

    expect(result?.status).toBe('error');
    expect(result?.nonFatalFailure).toBe(true);
  });

  it('keeps ACP read permission failures and missing diagnostics fatal', () => {
    for (const content of [
      undefined,
      [
        {
          type: 'content',
          content: { type: 'text', text: 'Permission denied (os error 13)' },
        },
      ],
    ]) {
      const result = normalizeAcpToolCall({
        type: 'acp_tool_call',
        id: 'msg-1',
        conversation_id: 1,
        content: {
          session_id: 'session-1',
          update: {
            sessionUpdate: 'tool_call_update',
            tool_call_id: 'tool-1',
            title: 'config.yaml',
            kind: 'read',
            status: 'failed',
            rawInput: { path: 'config.yaml' },
            content,
          },
        },
      } as any);

      expect(result?.nonFatalFailure).toBeUndefined();
    }
  });

  it('keeps non-shell ACP failures fatal for process receipts', () => {
    const result = normalizeAcpToolCall({
      type: 'acp_tool_call',
      id: 'msg-1',
      conversation_id: 1,
      content: {
        session_id: 'session-1',
        update: {
          sessionUpdate: 'tool_call_update',
          tool_call_id: 'tool-1',
          title: 'Fetch',
          kind: 'execute',
          status: 'failed',
          rawInput: {
            url: 'https://example.invalid',
          },
        },
      },
    } as any);

    expect(result?.status).toBe('error');
    expect(result?.nonFatalFailure).toBeUndefined();
  });
});
