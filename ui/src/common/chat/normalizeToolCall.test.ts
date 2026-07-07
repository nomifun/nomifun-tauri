import { describe, expect, it } from 'vitest';
import { normalizeAcpToolCall, normalizeToolCall } from './normalizeToolCall';

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

  it('marks failed ACP read/search probes as non-fatal process outcomes', () => {
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
        },
      },
    } as any);

    expect(result?.status).toBe('error');
    expect(result?.nonFatalFailure).toBe(true);
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
