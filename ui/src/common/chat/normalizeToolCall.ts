import type { IMessageAcpToolCall, IMessageToolCall, IMessageToolGroup } from './chatLib';
import { toDisplayText } from './displayText';

export type NormalizedToolStatus = 'pending' | 'running' | 'completed' | 'error' | 'canceled';

export interface NormalizedToolCall {
  key: string;
  name: string;
  status: NormalizedToolStatus;
  /** Tool reported an error-like outcome, but it should not fail the turn-level process receipt. */
  nonFatalFailure?: boolean;
  /** Tool was not executed because an earlier call in the same assistant turn failed. */
  skipped?: boolean;
  description?: string;
  input?: string;
  output?: string;
  truncated?: boolean;
  messageId?: string;
  conversationId?: number;
}

const formatValue = (value: unknown): string => toDisplayText(value);

// ===== tool_group → NormalizedToolCall[] =====

function normalizeToolGroupStatus(status: unknown): NormalizedToolStatus {
  switch (status) {
    case 'Success':
      return 'completed';
    case 'Error':
      return 'error';
    case 'Canceled':
      return 'canceled';
    case 'Pending':
      return 'pending';
    case 'Executing':
    case 'Confirming':
    default:
      return 'running';
  }
}

const getResultDisplayText = (
  result_display: IMessageToolGroup['content'][0]['result_display']
): string | undefined => {
  if (!result_display) return undefined;
  if (typeof result_display === 'string') return result_display;
  if ('file_diff' in result_display) return result_display.file_diff;
  if ('img_url' in result_display) return result_display.relative_path || result_display.img_url;
  return undefined;
};

export function normalizeToolGroup(message: IMessageToolGroup): NormalizedToolCall[] {
  if (!Array.isArray(message.content)) return [];
  return message.content.map(({ name, call_id, description, confirmationDetails, status, result_display }) => {
    let desc = typeof description === 'string' ? description.slice(0, 100) : '';
    // Guard on `confirmationDetails` so the discriminant `type` narrows the
    // union directly off the object; previously `type` was aliased through
    // optional chaining, which left `confirmationDetails` possibly-undefined.
    // The branches only ran when it was present before, so behavior is unchanged.
    if (confirmationDetails) {
      const type = confirmationDetails.type;
      if (type === 'edit') desc = toDisplayText(confirmationDetails.file_name);
      if (type === 'exec') desc = toDisplayText(confirmationDetails.command);
      if (type === 'info') {
        desc =
          confirmationDetails.urls?.map((url) => toDisplayText(url)).join(';') ||
          toDisplayText(confirmationDetails.title);
      }
      if (type === 'mcp') {
        desc = `${toDisplayText(confirmationDetails.server_name)}:${toDisplayText(confirmationDetails.tool_name)}`;
      }
    }

    let input: string | undefined;
    if (confirmationDetails) {
      const { title: _title, type: _type, ...rest } = confirmationDetails;
      if (Object.keys(rest).length) input = formatValue(rest);
    } else if (description) {
      input = description;
    }

    return {
      key: toDisplayText(call_id),
      name: toDisplayText(name, 'Tool'),
      status: normalizeToolGroupStatus(status),
      ...(status === 'Error' && confirmationDetails?.type === 'exec' ? { nonFatalFailure: true } : {}),
      description: desc,
      input,
      output: getResultDisplayText(result_display),
    };
  });
}

// ===== acp_tool_call → NormalizedToolCall =====

function normalizeAcpStatus(status: unknown): NormalizedToolStatus {
  switch (status) {
    case 'completed':
      return 'completed';
    case 'failed':
      return 'error';
    case 'in_progress':
      return 'running';
    case 'pending':
    default:
      return 'pending';
  }
}

const shellCommandTitles = new Set(['bash', 'shell', 'terminal', 'command', 'cmd', 'powershell']);
const shellCommandFieldNames = ['command', 'cmd', 'script', 'shell', 'bash'];

const pickStringField = (record: Record<string, unknown>, fields: string[]): string | undefined => {
  for (const field of fields) {
    const value = record[field];
    if (typeof value === 'string' && value.trim()) return value;
  }
  return undefined;
};

const pickShellCommandInput = (value: unknown): string | undefined => {
  if (!value || typeof value !== 'object') return undefined;
  const record = value as Record<string, unknown>;

  const direct = pickStringField(record, shellCommandFieldNames);
  if (direct) return direct;

  for (const fieldValue of Object.values(record)) {
    const nested = pickShellCommandInput(fieldValue);
    if (nested) return nested;
  }

  return undefined;
};

const hasShellCommandInput = (value: unknown): boolean => Boolean(pickShellCommandInput(value));

const isNonFatalAcpToolFailure = (
  update: AcpToolCallUpdateCompat,
  rawInput: Record<string, unknown> | undefined,
  output: string | undefined
): boolean => {
  if (update.status !== 'failed') return false;
  if (['read', 'glob', 'grep', 'search', 'find'].includes(update.kind)) {
    return isExplicitProbeMiss(update.kind, output);
  }
  if (update.kind !== 'execute') return false;
  if (hasShellCommandInput(rawInput)) return true;
  return shellCommandTitles.has(toDisplayText(update.title).trim().toLowerCase());
};

const buildParamSummary = (kind: string, rawInput?: Record<string, unknown>): string | undefined => {
  if (!rawInput) return undefined;

  if (kind === 'read' || kind === 'edit') {
    return pickStringField(rawInput, ['file_path', 'path', 'file_name']);
  }
  if (kind === 'execute') {
    return pickShellCommandInput(rawInput.command) || pickShellCommandInput(rawInput);
  }
  if (kind === 'search' || kind === 'grep') {
    const parts: string[] = [];
    if (rawInput.pattern) parts.push(`"${rawInput.pattern}"`);
    if (rawInput.path) parts.push(`in ${rawInput.path}`);
    else if (rawInput.glob) parts.push(`in ${rawInput.glob}`);
    return parts.length > 0 ? parts.join(' ') : undefined;
  }
  if (kind === 'glob') {
    const parts: string[] = [];
    if (rawInput.pattern) parts.push(`${rawInput.pattern}`);
    if (rawInput.path) parts.push(`in ${rawInput.path}`);
    return parts.length > 0 ? parts.join(' ') : undefined;
  }
  if (kind === 'write') {
    return pickStringField(rawInput, ['file_path', 'path']);
  }

  for (const key of ['file_path', 'command', 'path', 'pattern', 'query', 'url']) {
    if (rawInput[key] && typeof rawInput[key] === 'string') return rawInput[key] as string;
  }
  return undefined;
};

type AcpToolCallUpdateCompat = IMessageAcpToolCall['content']['update'] & {
  session_update?: string;
  raw_input?: Record<string, unknown>;
};

type AcpToolCallContentCompat = IMessageAcpToolCall['content'] & {
  _compact?: {
    truncated?: boolean;
    original_size?: number;
    preview_chars?: number;
  };
  update?: AcpToolCallUpdateCompat;
};

export function normalizeAcpToolCall(message: IMessageAcpToolCall): NormalizedToolCall | undefined {
  const content = message.content as AcpToolCallContentCompat | undefined;
  const update = content?.update;
  if (!update) return undefined;

  const rawInput = update.rawInput ?? update.raw_input;
  const input = rawInput ? formatValue(rawInput) : undefined;

  let output: string | undefined;
  if (Array.isArray(update.content) && update.content.length) {
    output = update.content
      .map((item) => {
        if (item.type === 'content' && item.content?.text) return item.content.text;
        if (item.type === 'diff' && 'path' in item) return `[diff] ${item.path}`;
        return '';
      })
      .filter(Boolean)
      .join('\n');
  }

  const kind = toDisplayText(update.kind, 'execute');
  const keyParam = buildParamSummary(kind, rawInput);
  const commandText = pickShellCommandInput(rawInput);

  return {
    key: toDisplayText(update.tool_call_id),
    name: toDisplayText(update.title, 'Tool'),
    status: normalizeAcpStatus(update.status),
    ...(isNonFatalAcpToolFailure(update, rawInput, output) ? { nonFatalFailure: true } : {}),
    description: keyParam || commandText || kind,
    input,
    output,
    truncated: content?._compact?.truncated === true,
    messageId: message.id,
    conversationId: message.conversation_id,
  };
}

// ===== tool_call → NormalizedToolCall =====

function normalizeToolCallStatus(status?: unknown): NormalizedToolStatus {
  switch (status) {
    case 'completed':
      return 'completed';
    case 'error':
      return 'error';
    case 'running':
      return 'running';
    default:
      return 'pending';
  }
}

const isOrdinaryShellExit = (name: unknown, status: unknown, output: unknown): boolean => {
  if (status !== 'error') return false;
  if (!shellCommandTitles.has(toDisplayText(name).trim().toLowerCase())) return false;

  const text = toDisplayText(output);
  const match = /^Exit code:\s*(-?\d+)(?:\r?\n|$)/.exec(text);
  if (!match) return false;

  const exitCode = Number(match[1]);
  return (
    Number.isInteger(exitCode) &&
    exitCode !== 0 &&
    exitCode !== -1 &&
    !/(?:^|\r?\n)Signal:/m.test(text) &&
    !/(?:^|\r?\n)Cleanup diagnostics:/m.test(text)
  );
};

const directProbeToolTitles = new Set(['read', 'glob', 'grep', 'search', 'find']);

const isExplicitProbeMiss = (name: unknown, output: unknown): boolean => {
  const toolName = toDisplayText(name).trim().toLowerCase();
  const text = toDisplayText(output).trim();
  if (!text || text.startsWith('The turn ended before this tool completed:')) return false;

  if (toolName === 'read') {
    return (
      /\(os error (?:2|3)\)/i.test(text) ||
      /no such file or directory/i.test(text) ||
      /(?:system )?cannot find the (?:file|path)/i.test(text)
    );
  }

  if (toolName === 'glob') return /^No files matched the pattern\.?$/i.test(text);
  return /^(?:No matches found|No files matched the pattern)\.?$/i.test(text);
};

const isOrdinaryDirectProbeFailure = (name: unknown, status: unknown, output: unknown): boolean => {
  if (status !== 'error') return false;
  if (!directProbeToolTitles.has(toDisplayText(name).trim().toLowerCase())) return false;
  return isExplicitProbeMiss(name, output);
};

const skippedAfterPriorErrorPrefix = 'Skipped because a previous tool call in this assistant turn failed.';

const isSkippedAfterPriorError = (status: unknown, output: unknown): boolean =>
  status === 'error' && toDisplayText(output).trimStart().startsWith(skippedAfterPriorErrorPrefix);

export function normalizeToolCall(message: IMessageToolCall): NormalizedToolCall | undefined {
  const { call_id, name, status, input, output, args, description } = message.content;
  if (!call_id) return undefined;

  const displayInput = input
    ? formatValue(input)
    : args && Object.keys(args).length > 0
      ? formatValue(args)
      : undefined;
  const skipped = isSkippedAfterPriorError(status, output);

  return {
    key: toDisplayText(call_id),
    name: toDisplayText(name, 'Tool'),
    status: skipped ? 'canceled' : normalizeToolCallStatus(status),
    ...(skipped ? { skipped: true } : {}),
    ...(isOrdinaryShellExit(name, status, output) || isOrdinaryDirectProbeFailure(name, status, output)
      ? { nonFatalFailure: true }
      : {}),
    description: description ? formatValue(description) : undefined,
    input: displayInput,
    output: output ? toDisplayText(output) : undefined,
  };
}

// ===== Unified entry =====

export type ToolMessage = IMessageToolGroup | IMessageAcpToolCall | IMessageToolCall;

export function normalizeToolMessages(messages: ToolMessage[]): NormalizedToolCall[] {
  return messages
    .flatMap((m) => {
      if (m.type === 'tool_group') return normalizeToolGroup(m);
      if (m.type === 'acp_tool_call') return normalizeAcpToolCall(m);
      if (m.type === 'tool_call') return normalizeToolCall(m);
      return undefined;
    })
    .filter((item): item is NormalizedToolCall => item !== undefined);
}

export function hasRunningToolMessages(messages: ToolMessage[]): boolean {
  return messages.some((m) => {
    if (m.type === 'tool_group') {
      return Array.isArray(m.content) && m.content.some((t) => normalizeToolGroupStatus(t.status) === 'running');
    }
    if (m.type === 'acp_tool_call') {
      return m.content?.update && normalizeAcpStatus(m.content.update.status) === 'running';
    }
    if (m.type === 'tool_call') {
      return normalizeToolCallStatus(m.content?.status) === 'running';
    }
    return false;
  });
}
