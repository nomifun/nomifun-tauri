/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type TurnDisclosureRole = 'user' | 'assistant' | 'process' | 'process_content' | 'other';
export type TurnDisclosureProcessState = 'completed' | 'running' | 'waiting' | 'failed' | 'canceled';

export interface TurnDisclosureInputItem {
  id: string;
  turnId?: string;
  role: TurnDisclosureRole;
  createdAt: number;
  processStartedAt?: number;
  processEndedAt?: number;
  processState?: TurnDisclosureProcessState;
  running?: boolean;
  sourceMessageIds?: string[];
}

export type TurnDisclosureOutputItem =
  | { type: 'item'; id: string }
  | { type: 'process_receipt'; id: string; itemId: string }
  | {
      type: 'turn_disclosure';
      id: string;
      turnId: string;
      processItemIds: string[];
      sourceMessageIds: string[];
      startAt: number;
      endAt: number;
      state: TurnDisclosureProcessState;
      processItemStates: Record<string, TurnDisclosureProcessState>;
      running: boolean;
      defaultCollapsed: boolean;
    };

export interface BuildTurnDisclosureOptions {
  tailClosed?: boolean;
}

const unique = (values: string[]): string[] => Array.from(new Set(values.filter(Boolean)));

const toProcessReceipt = (entry: TurnDisclosureInputItem): TurnDisclosureOutputItem => ({
  type: 'process_receipt',
  id: `receipt-${entry.id}`,
  itemId: entry.id,
});

export function assignTurnIdsFromUserRequests(items: TurnDisclosureInputItem[]): TurnDisclosureInputItem[] {
  let currentTurnId: string | undefined;

  return items.map((entry) => {
    if (entry.role === 'user') {
      currentTurnId = entry.turnId || entry.id;
      return { ...entry, turnId: currentTurnId };
    }

    if (!currentTurnId) {
      return { ...entry, turnId: undefined };
    }

    return { ...entry, turnId: currentTurnId };
  });
}

const getProcessState = (entry: TurnDisclosureInputItem): TurnDisclosureProcessState => {
  if (entry.processState) return entry.processState;
  if (entry.running) return 'running';
  return 'completed';
};

const getEffectiveProcessState = (
  entry: TurnDisclosureInputItem,
  options: { isClosed: boolean; hasFinalAssistant: boolean }
): TurnDisclosureProcessState => {
  const state = getProcessState(entry);
  if (options.isClosed && options.hasFinalAssistant && (state === 'running' || state === 'waiting')) {
    return 'completed';
  }
  return state;
};

const getProcessStartAt = (entry: TurnDisclosureInputItem): number => entry.processStartedAt ?? entry.createdAt;

const getProcessEndAt = (entry: TurnDisclosureInputItem): number => entry.processEndedAt ?? entry.createdAt;

const resolveDisclosureState = (
  processItems: TurnDisclosureInputItem[],
  options: { isClosed: boolean; hasFinalAssistant: boolean }
): TurnDisclosureProcessState => {
  const states = processItems.map((entry) => getEffectiveProcessState(entry, options));
  if (states.includes('waiting')) return 'waiting';
  if (states.includes('running')) return 'running';
  if (states.includes('failed')) return 'failed';
  if (states.includes('canceled')) return 'canceled';
  return 'completed';
};

const buildEmptyRunningDisclosure = (
  turnId: string,
  segment: TurnDisclosureInputItem[]
): TurnDisclosureOutputItem => {
  const startEntry = segment.findLast((entry) => entry.role === 'user') ?? segment[0];
  const startAt = startEntry ? getProcessStartAt(startEntry) : 0;
  const endAt = segment.length ? Math.max(...segment.map(getProcessEndAt)) : startAt;

  return {
    type: 'turn_disclosure',
    id: `turn-disclosure-${turnId}`,
    turnId,
    processItemIds: [],
    sourceMessageIds: [],
    startAt,
    endAt,
    state: 'running',
    processItemStates: {},
    running: true,
    defaultCollapsed: false,
  };
};

const buildEmptyRunningSegmentOutput = (
  segment: TurnDisclosureInputItem[],
  disclosure: TurnDisclosureOutputItem
): TurnDisclosureOutputItem[] => {
  const output: TurnDisclosureOutputItem[] = [];
  let insertedDisclosure = false;

  segment.forEach((entry) => {
    if (!insertedDisclosure && entry.role !== 'user' && entry.role !== 'other') {
      output.push(disclosure);
      insertedDisclosure = true;
    }
    output.push({ type: 'item', id: entry.id });
  });

  if (!insertedDisclosure) {
    output.push(disclosure);
  }

  return output;
};

function buildSegmentOutput(segment: TurnDisclosureInputItem[], isClosed: boolean): TurnDisclosureOutputItem[] {
  const turnId = segment[0]?.turnId;
  if (!turnId) return segment.map((entry) => ({ type: 'item', id: entry.id }));

  const finalAssistantIndex = segment.findLastIndex((entry) => entry.role === 'assistant');
  const stateOptions = { isClosed, hasFinalAssistant: finalAssistantIndex !== -1 };

  const processItems = segment.filter((entry, index) => {
    if (entry.role === 'user' || entry.role === 'other') return false;
    return index !== finalAssistantIndex;
  });

  if (!processItems.length) {
    if (!isClosed) {
      return buildEmptyRunningSegmentOutput(segment, buildEmptyRunningDisclosure(turnId, segment));
    }
    return segment.map((entry) => ({ type: 'item', id: entry.id }));
  }

  const resolvedState = resolveDisclosureState(processItems, stateOptions);
  const state = !isClosed && resolvedState === 'completed' ? 'running' : resolvedState;

  const finalOrProcessItems =
    finalAssistantIndex === -1 ? processItems : [...processItems, segment[finalAssistantIndex]].filter(Boolean);
  const disclosure: TurnDisclosureOutputItem = {
    type: 'turn_disclosure',
    id: `turn-disclosure-${turnId}`,
    turnId,
    processItemIds: processItems.map((entry) => entry.id),
    sourceMessageIds: unique(processItems.flatMap((entry) => entry.sourceMessageIds ?? [entry.id])),
    startAt: Math.min(...processItems.map(getProcessStartAt)),
    endAt: Math.max(...finalOrProcessItems.map(getProcessEndAt)),
    state,
    processItemStates: Object.fromEntries(
      processItems.map((entry) => [entry.id, getEffectiveProcessState(entry, stateOptions)])
    ),
    running: state === 'running' || state === 'waiting',
    defaultCollapsed: state !== 'running' && state !== 'waiting',
  };

  const output: TurnDisclosureOutputItem[] = [];
  let insertedDisclosure = false;

  segment.forEach((entry, index) => {
    if (entry.role !== 'user' && entry.role !== 'other' && index !== finalAssistantIndex) {
      return;
    }

    if (index === finalAssistantIndex && !insertedDisclosure) {
      output.push(disclosure);
      insertedDisclosure = true;
    }

    output.push({ type: 'item', id: entry.id });
  });

  if (!insertedDisclosure) {
    output.push(disclosure);
  }

  return output;
}

export function buildTurnDisclosureItems(
  items: TurnDisclosureInputItem[],
  options: BuildTurnDisclosureOptions = {}
): TurnDisclosureOutputItem[] {
  const output: TurnDisclosureOutputItem[] = [];
  let segment: TurnDisclosureInputItem[] = [];

  const flush = (isClosed: boolean) => {
    if (!segment.length) return;
    output.push(...buildSegmentOutput(segment, isClosed));
    segment = [];
  };

  for (const item of items) {
    if (!item.turnId) {
      flush(true);
      output.push(item.role === 'process' ? toProcessReceipt(item) : { type: 'item', id: item.id });
      continue;
    }

    const currentTurnId = segment[0]?.turnId;
    if (currentTurnId && currentTurnId !== item.turnId) {
      flush(true);
    }

    segment.push(item);
  }

  flush(options.tailClosed === true);
  return output;
}
