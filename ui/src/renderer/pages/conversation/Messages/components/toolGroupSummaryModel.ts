/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import {
  formatToolDisplayName,
  type NormalizedToolCall,
  type NormalizedToolNotExecutedReason,
} from '@/common/chat/normalizeToolCall';
import type { TurnDisclosureProcessState } from '../turnDisclosureModel';
import { mergeProcessStates } from '../turnProcessState';

export interface ToolSummaryDescriptor {
  target: string;
  count: number;
}

export type ToolReceiptAction =
  | 'read_files'
  | 'edit_files'
  | 'run_commands'
  | 'search_code'
  | 'list_files'
  | 'load_tools'
  | 'generic';

export type ToolReceiptIcon = 'tool' | 'file' | 'edit';

export interface ToolReceiptSummaryPart {
  action: ToolReceiptAction;
  count: number;
  state: TurnDisclosureProcessState;
  target?: string;
  skipped?: boolean;
  notExecutedReason?: NormalizedToolNotExecutedReason;
}

export interface ToolReceiptDetailRow {
  key: string;
  action: ToolReceiptAction;
  state: TurnDisclosureProcessState;
  title: string;
  target?: string;
  input?: string;
  output?: string;
  truncated?: boolean;
  skipped?: boolean;
  notExecutedReason?: NormalizedToolNotExecutedReason;
}

const toolReceiptIconByAction: Record<ToolReceiptAction, ToolReceiptIcon> = {
  read_files: 'file',
  edit_files: 'edit',
  run_commands: 'tool',
  search_code: 'file',
  list_files: 'file',
  load_tools: 'tool',
  generic: 'tool',
};

const stateMatchesTool = (state: TurnDisclosureProcessState, tool: NormalizedToolCall): boolean => {
  if (state === 'running') return tool.status === 'running' || tool.status === 'pending';
  if (state === 'failed') return tool.status === 'error' && !tool.nonFatalFailure;
  if (state === 'canceled') return tool.status === 'canceled';
  if (state === 'completed') return tool.status === 'completed' || tool.nonFatalFailure === true;
  return tool.status === 'pending' || tool.status === 'running';
};

const compactToolText = (value?: unknown): string => {
  if (value == null) return '';
  const text =
    typeof value === 'string'
      ? value
      : (() => {
          try {
            return JSON.stringify(value, null, 2);
          } catch {
            return String(value);
          }
        })();
  return text.replace(/\s+/g, ' ').trim();
};

const formatToolTarget = (tool: NormalizedToolCall): string => {
  if (classifyToolForReceipt(tool) === 'run_commands') return getCommandTarget(tool);

  const rawName = compactToolText(tool.name);
  const name = compactToolText(formatToolDisplayName(rawName));
  const description = compactToolText(tool.description);
  if (name && description && description !== name && description !== rawName) return `${name} ${description}`;
  return name || description || tool.key;
};

const commandFieldNames = ['command', 'cmd', 'script', 'shell', 'bash'];
const fileFieldNames = ['file_path', 'filePath', 'path', 'file_name', 'fileName', 'relative_path', 'relativePath'];

const pickCommandFromValue = (value: unknown): string | undefined => {
  if (!value || typeof value !== 'object') return undefined;
  const record = value as Record<string, unknown>;

  for (const field of commandFieldNames) {
    const fieldValue = record[field];
    if (typeof fieldValue === 'string' && compactToolText(fieldValue)) return compactToolText(fieldValue);
  }

  for (const fieldValue of Object.values(record)) {
    if (fieldValue && typeof fieldValue === 'object') {
      const nested = pickCommandFromValue(fieldValue);
      if (nested) return nested;
    }
  }

  return undefined;
};

const extractCommandFromText = (value?: string): string | undefined => {
  const compacted = compactToolText(value);
  if (!compacted) return undefined;

  try {
    const parsed = JSON.parse(value ?? '');
    const command = pickCommandFromValue(parsed);
    if (command) return command;
    if (typeof parsed === 'string') return compactToolText(parsed);
    return undefined;
  } catch {
    // Plain shell strings are already the desired preview.
  }

  return compacted;
};

const pickFileTargetFromValue = (value: unknown): string | undefined => {
  if (!value || typeof value !== 'object') return undefined;
  const record = value as Record<string, unknown>;

  for (const field of fileFieldNames) {
    const fieldValue = record[field];
    if (typeof fieldValue === 'string' && compactToolText(fieldValue)) return compactToolText(fieldValue);
  }

  for (const fieldValue of Object.values(record)) {
    if (fieldValue && typeof fieldValue === 'object') {
      const nested = pickFileTargetFromValue(fieldValue);
      if (nested) return nested;
    }
  }

  return undefined;
};

const extractFileTargetFromText = (value?: string): string | undefined => {
  const compacted = compactToolText(value);
  if (!compacted) return undefined;

  try {
    const parsed = JSON.parse(value ?? '');
    const target = pickFileTargetFromValue(parsed);
    if (target) return target;
    if (typeof parsed === 'string') return compactToolText(parsed);
    return undefined;
  } catch {
    // Plain read/edit descriptions are already useful file previews.
  }

  return compacted;
};

const getCommandTarget = (tool: NormalizedToolCall): string => {
  const description = compactToolText(tool.description);
  const rawName = compactToolText(tool.name);
  const name = compactToolText(formatToolDisplayName(rawName));
  if (description && description !== name && description !== rawName) return description;
  return extractCommandFromText(tool.input) || description || name || tool.key;
};

const getFileTarget = (tool: NormalizedToolCall): string | undefined => {
  const description = compactToolText(tool.description);
  const rawName = compactToolText(tool.name);
  const name = compactToolText(formatToolDisplayName(rawName));
  if (description && description !== name && description !== rawName) return description;
  return extractFileTargetFromText(tool.input);
};

const normalizeToolSearchText = (value: string): string => value.replace(/[_-]+/g, ' ').toLowerCase();

// Receipt categories describe concrete UI affordances, so classify only tool
// names whose semantics we actually know. Keyword matching arbitrary MCP names
// (for example `nomi_knowledge_update_base`) incorrectly presented domain
// operations as file edits.
const commandToolNames = new Set(['bash', 'shell', 'exec', 'exec command', 'execute command', 'run command', 'terminal']);
const codeSearchToolNames = new Set(['grep', 'rg', 'search', 'find']);
const fileListToolNames = new Set(['glob', 'list', 'ls', 'directory', 'dir']);
const fileEditToolNames = new Set(['write', 'edit', 'patch', 'apply patch', 'multi edit', 'replace']);
const fileReadToolNames = new Set(['read', 'open', 'view', 'cat']);
const toolLoaderNames = new Set(['toolsearch', 'tool search']);

const commandActionTokens = new Set(['exec', 'execute', 'run']);
const commandSuffixes = new Set(['command', 'commands']);
const codeSearchActionTokens = new Set(['grep', 'rg', 'search', 'find']);
const codeSearchSuffixes = new Set(['code', 'file', 'files']);
const fileListActionTokens = new Set(['glob', 'list', 'ls']);
const fileListSuffixes = new Set(['file', 'files', 'directory', 'directories', 'entry', 'entries']);
const fileEditActionTokens = new Set(['write', 'edit', 'patch', 'replace', 'apply', 'multi']);
const fileEditSuffixes = new Set(['file', 'files', 'patch', 'edit']);
const fileReadActionTokens = new Set(['read', 'open', 'view', 'cat']);
const fileReadSuffixes = new Set(['file', 'files']);

interface ToolActionName {
  name: string;
  isMcp: boolean;
}

const getNamespacedToolActionName = (value?: unknown): ToolActionName => {
  const fullName = compactToolText(value);
  const segments = fullName.split('__');
  const isMcp = fullName.startsWith('mcp__');
  // Stable MCP provider names end in a 16-character base32 origin hash. It is
  // routing metadata, not the tool action; remove only that exact canonical
  // suffix so legacy/non-MCP names cannot be reinterpreted by accident.
  if (isMcp && segments.length >= 4 && /^[a-z2-7]{16}$/.test(segments.at(-1) ?? '')) {
    segments.pop();
  }
  const leafName = segments.at(-1) ?? fullName;
  return { name: normalizeToolSearchText(leafName).trim(), isMcp };
};

const matchesAnchoredToolAction = (
  name: string,
  exactNames: Set<string>,
  actionTokens: Set<string>,
  suffixes: Set<string>,
  allowExactName: boolean
): boolean => {
  if (allowExactName && exactNames.has(name)) return true;
  const [action, ...suffixParts] = name.split(' ').filter(Boolean);
  return actionTokens.has(action) && suffixParts.length > 0 && suffixes.has(suffixParts.join(' '));
};

const classifyToolForReceipt = (tool: NormalizedToolCall): ToolReceiptAction => {
  const kind = normalizeToolSearchText(compactToolText(tool.kind)).trim();
  const { name, isMcp } = getNamespacedToolActionName(tool.name);

  if (kind === 'execute') return 'run_commands';
  if (['search', 'grep', 'find'].includes(kind)) return 'search_code';
  if (['glob', 'list'].includes(kind)) return 'list_files';
  if (['edit', 'write'].includes(kind)) return 'edit_files';
  if (kind === 'read') return 'read_files';
  // A server-local MCP name is descriptive metadata, not a trusted semantic
  // kind. Preserve strong compound actions such as read_file/exec_command, but
  // do not turn ambiguous one-word names such as search/read/run/list into
  // code, file, or command receipts. Native built-ins keep their exact-name UI.
  const allowExactName = !isMcp;
  if (allowExactName && toolLoaderNames.has(name)) return 'load_tools';
  if (
    matchesAnchoredToolAction(
      name,
      commandToolNames,
      commandActionTokens,
      commandSuffixes,
      allowExactName
    )
  ) {
    return 'run_commands';
  }
  if (
    matchesAnchoredToolAction(
      name,
      codeSearchToolNames,
      codeSearchActionTokens,
      codeSearchSuffixes,
      allowExactName
    )
  ) {
    return 'search_code';
  }
  if (
    matchesAnchoredToolAction(
      name,
      fileListToolNames,
      fileListActionTokens,
      fileListSuffixes,
      allowExactName
    )
  ) {
    return 'list_files';
  }
  if (
    matchesAnchoredToolAction(
      name,
      fileEditToolNames,
      fileEditActionTokens,
      fileEditSuffixes,
      allowExactName
    )
  ) {
    return 'edit_files';
  }
  if (
    matchesAnchoredToolAction(
      name,
      fileReadToolNames,
      fileReadActionTokens,
      fileReadSuffixes,
      allowExactName
    )
  ) {
    return 'read_files';
  }
  return 'generic';
};

const getToolReceiptTarget = (tool: NormalizedToolCall, action: ToolReceiptAction): string | undefined => {
  if (action === 'run_commands') {
    return getCommandTarget(tool);
  }
  if (action === 'read_files' || action === 'edit_files') {
    return getFileTarget(tool);
  }
  if (action !== 'generic') return undefined;
  return formatToolTarget(tool);
};

const getToolReceiptDetailTarget = (tool: NormalizedToolCall, action: ToolReceiptAction): string | undefined => {
  const description = compactToolText(tool.description);
  const rawName = compactToolText(tool.name);
  const name = compactToolText(formatToolDisplayName(rawName));

  if (action === 'generic') return formatToolTarget(tool);
  if (action === 'read_files' || action === 'edit_files') return getFileTarget(tool);
  if (description && description !== name && description !== rawName) return description;
  if (action === 'run_commands') return getCommandTarget(tool);
  return undefined;
};

const getToolProcessState = (tool: NormalizedToolCall): TurnDisclosureProcessState => {
  if (tool.status === 'running' || tool.status === 'pending') return 'running';
  // Receipt state controls color and turn health. The dedicated copy carries
  // the "not executed" meaning without presenting a local preflight rejection
  // as a user-visible task failure/cancellation.
  if (tool.notExecutedReason) return 'completed';
  if (tool.nonFatalFailure) return 'completed';
  if (tool.status === 'error') return 'failed';
  if (tool.status === 'canceled') return 'canceled';
  return 'completed';
};

export const buildToolReceiptSummaryParts = (
  tools: NormalizedToolCall[],
  _state: TurnDisclosureProcessState
): ToolReceiptSummaryPart[] => {
  const grouped = new Map<
    string,
    {
      action: ToolReceiptAction;
      count: number;
      skippedCount: number;
      targets: string[];
      states: TurnDisclosureProcessState[];
      notExecutedReason?: NormalizedToolNotExecutedReason;
    }
  >();

  tools.forEach((tool) => {
    const action = classifyToolForReceipt(tool);
    const target = getToolReceiptTarget(tool, action);
    // A pre-dispatch rejection is a different receipt outcome from a tool that
    // actually ran. Keep them separate even when their semantic action matches.
    const groupKey = `${action}:${tool.notExecutedReason ?? 'executed'}`;
    const current = grouped.get(groupKey) ?? {
      action,
      count: 0,
      skippedCount: 0,
      targets: [],
      states: [],
      ...(tool.notExecutedReason ? { notExecutedReason: tool.notExecutedReason } : {}),
    };
    current.count += 1;
    if (tool.skipped) current.skippedCount += 1;
    current.states.push(getToolProcessState(tool));
    if (target) current.targets.push(target);
    grouped.set(groupKey, current);
  });

  return Array.from(grouped.values()).map((value) => ({
    action: value.action,
    count: value.count,
    state: mergeProcessStates(value.states),
    ...(value.targets.length ? { target: Array.from(new Set(value.targets)).join(', ') } : {}),
    ...(value.skippedCount === value.count ? { skipped: true } : {}),
    ...(value.notExecutedReason ? { notExecutedReason: value.notExecutedReason } : {}),
  }));
};

export const getToolReceiptIconFromSummaryParts = (parts: ToolReceiptSummaryPart[]): ToolReceiptIcon | undefined => {
  const focusedPart =
    parts.findLast((part) => part.state === 'running' || part.state === 'waiting') ??
    parts.findLast((part) => part.state === 'failed' || part.state === 'canceled') ??
    parts.at(-1);
  return focusedPart ? toolReceiptIconByAction[focusedPart.action] : undefined;
};

export const buildToolReceiptDetailRows = (tools: NormalizedToolCall[]): ToolReceiptDetailRow[] =>
  tools.map((tool) => {
    const action = classifyToolForReceipt(tool);
    const title = compactToolText(formatToolDisplayName(tool.name)) || tool.key;
    const target = getToolReceiptDetailTarget(tool, action);
    return {
      key: tool.key,
      action,
      state: getToolProcessState(tool),
      title,
      ...(target ? { target } : {}),
      ...(tool.input ? { input: tool.input } : {}),
      ...(tool.output ? { output: tool.output } : {}),
      ...(tool.truncated ? { truncated: tool.truncated } : {}),
      ...(tool.skipped ? { skipped: true } : {}),
      ...(tool.notExecutedReason ? { notExecutedReason: tool.notExecutedReason } : {}),
    };
  });

export const buildToolSummaryDescriptor = (
  tools: NormalizedToolCall[],
  state: TurnDisclosureProcessState
): ToolSummaryDescriptor | null => {
  if (!tools.length) return null;

  const focusedTool = tools.findLast((tool) => stateMatchesTool(state, tool)) ?? tools.at(-1);
  if (!focusedTool) return null;

  return {
    target: formatToolTarget(focusedTool),
    count: tools.length,
  };
};
