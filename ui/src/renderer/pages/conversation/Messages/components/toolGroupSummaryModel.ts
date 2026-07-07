/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { NormalizedToolCall } from '@/common/chat/normalizeToolCall';
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
  if (state === 'failed') return tool.status === 'error';
  if (state === 'canceled') return tool.status === 'canceled';
  if (state === 'completed') return tool.status === 'completed';
  return tool.status === 'pending' || tool.status === 'running';
};

const compactToolText = (value?: string): string => value?.replace(/\s+/g, ' ').trim() ?? '';

const formatToolTarget = (tool: NormalizedToolCall): string => {
  if (classifyToolForReceipt(tool) === 'run_commands') return getCommandTarget(tool);

  const name = tool.name?.trim();
  const description = tool.description?.trim();
  if (name && description && description !== name) return `${name} ${description}`;
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
  const name = compactToolText(tool.name);
  if (description && description !== name) return description;
  return extractCommandFromText(tool.input) || description || name || tool.key;
};

const getFileTarget = (tool: NormalizedToolCall): string | undefined => {
  const description = compactToolText(tool.description);
  const name = compactToolText(tool.name);
  if (description && description !== name) return description;
  return extractFileTargetFromText(tool.input);
};

const normalizeToolSearchText = (value: string): string => value.replace(/[_-]+/g, ' ').toLowerCase();

const getToolSearchText = (tool: NormalizedToolCall): string =>
  normalizeToolSearchText(`${tool.name ?? ''} ${tool.description ?? ''} ${tool.key ?? ''}`);

const getToolNameSearchText = (tool: NormalizedToolCall): string =>
  normalizeToolSearchText(`${tool.name ?? ''} ${tool.key ?? ''}`);

const classifyToolForReceipt = (tool: NormalizedToolCall): ToolReceiptAction => {
  const text = getToolSearchText(tool);
  const nameText = getToolNameSearchText(tool);

  if (/\b(bash|shell|exec|execute|terminal|command|run)\b/.test(nameText)) return 'run_commands';
  if (/\b(grep|rg|search|find)\b/.test(text)) return 'search_code';
  if (/\b(glob|list|ls|directory|dir)\b/.test(text)) return 'list_files';
  if (/\b(write|edit|patch|update|modify|replace)\b/.test(text)) return 'edit_files';
  if (/\b(read|open|view|cat)\b/.test(text)) return 'read_files';
  if (/\b(bash|shell|exec|execute|terminal|command|run)\b/.test(text)) return 'run_commands';
  if (/\b(load|loaded)\b.*\btools?\b/.test(text)) return 'load_tools';
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
  const name = compactToolText(tool.name);

  if (action === 'generic') return formatToolTarget(tool);
  if (action === 'read_files' || action === 'edit_files') return getFileTarget(tool);
  if (description && description !== name) return description;
  if (action === 'run_commands') return getCommandTarget(tool);
  return undefined;
};

const getToolProcessState = (tool: NormalizedToolCall): TurnDisclosureProcessState => {
  if (tool.status === 'running' || tool.status === 'pending') return 'running';
  if (tool.status === 'error') return 'failed';
  if (tool.status === 'canceled') return 'canceled';
  return 'completed';
};

export const buildToolReceiptSummaryParts = (
  tools: NormalizedToolCall[],
  _state: TurnDisclosureProcessState
): ToolReceiptSummaryPart[] => {
  const grouped = new Map<ToolReceiptAction, { count: number; targets: string[]; states: TurnDisclosureProcessState[] }>();

  tools.forEach((tool) => {
    const action = classifyToolForReceipt(tool);
    const target = getToolReceiptTarget(tool, action);
    const current = grouped.get(action) ?? { count: 0, targets: [], states: [] };
    current.count += 1;
    current.states.push(getToolProcessState(tool));
    if (target) current.targets.push(target);
    grouped.set(action, current);
  });

  return Array.from(grouped.entries()).map(([action, value]) => ({
    action,
    count: value.count,
    state: mergeProcessStates(value.states),
    ...(value.targets.length ? { target: Array.from(new Set(value.targets)).join(', ') } : {}),
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
    const title = compactToolText(tool.name) || tool.key;
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
