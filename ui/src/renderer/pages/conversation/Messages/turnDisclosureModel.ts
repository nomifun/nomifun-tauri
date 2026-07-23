/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { MessageId } from '@/common/types/ids';

export type TurnDisclosureRole = 'user' | 'assistant' | 'process' | 'process_content' | 'other';
export type TurnDisclosureProcessState = 'completed' | 'running' | 'waiting' | 'failed' | 'canceled';

export interface TurnDisclosureInputItem {
  id: string;
  turnId?: MessageId;
  role: TurnDisclosureRole;
  createdAt: number;
  processStartedAt?: number;
  processEndedAt?: number;
  processState?: TurnDisclosureProcessState;
  running?: boolean;
  sourceMessageIds?: MessageId[];
}

export type TurnDisclosureOutputItem =
  | { type: 'item'; id: string }
  | { type: 'process_receipt'; id: string; itemId: string }
  | {
      type: 'turn_disclosure';
      id: string;
      turnId: MessageId;
      processItemIds: string[];
      sourceMessageIds: MessageId[];
      startAt: number;
      endAt: number;
      state: TurnDisclosureProcessState;
      processItemStates: Record<string, TurnDisclosureProcessState>;
      running: boolean;
      defaultCollapsed: boolean;
    };

export interface BuildTurnDisclosureOptions {
  tailClosed?: boolean;
  activeTurnId?: MessageId;
}

export interface AssignTurnIdOptions {
  activeTurnId?: MessageId;
  activeRequestMessageId?: MessageId;
}

const unique = <T extends string>(values: T[]): T[] => Array.from(new Set(values.filter(Boolean)));

const toProcessReceipt = (entry: TurnDisclosureInputItem): TurnDisclosureOutputItem => ({
  type: 'process_receipt',
  id: `receipt-${entry.id}`,
  itemId: entry.id,
});

export function assignTurnIdsFromUserRequests(
  items: TurnDisclosureInputItem[],
  options: AssignTurnIdOptions = {}
): TurnDisclosureInputItem[] {
  const output: TurnDisclosureInputItem[] = [];
  let currentTurnId: MessageId | undefined;
  let authoritativeTurnId: MessageId | undefined;
  let requestBoundaryTurnId: MessageId | undefined;
  let currentRequestFallbackIndices: number[] = [];
  const retiredTurnIds = new Set<MessageId>();

  for (const entry of items) {
    if (entry.role === 'user') {
      if (requestBoundaryTurnId) retiredTurnIds.add(requestBoundaryTurnId);
      if (currentTurnId) retiredTurnIds.add(currentTurnId);
      if (authoritativeTurnId) retiredTurnIds.add(authoritativeTurnId);
      requestBoundaryTurnId = entry.turnId;
      const activeRootTurnId =
        options.activeTurnId &&
        options.activeRequestMessageId &&
        entry.turnId === options.activeRequestMessageId
          ? options.activeTurnId
          : undefined;
      // Once turn.started has correlated the visible request with the backend
      // root, that pair is stronger evidence than positional stream order.
      // In particular, an unseen delayed event from an older turn must not
      // claim the new request merely because it is the first explicit id.
      currentTurnId = activeRootTurnId ?? entry.turnId;
      authoritativeTurnId = activeRootTurnId;
      currentRequestFallbackIndices = [output.length];
      output.push({ ...entry, turnId: currentTurnId });
      continue;
    }

    // New stream/history rows carry the authoritative owning turn. Preserve it
    // even when a delayed event is interleaved after a newer user request. A
    // user's message id is only a provisional request boundary: ACP backends
    // mint a distinct root turn id for the response. The first non-retired
    // explicit id therefore becomes the fallback for later transient rows that
    // omit turn_id. Known older ids must not move that boundary back.
    if (entry.turnId) {
      if (!authoritativeTurnId && !retiredTurnIds.has(entry.turnId)) {
        authoritativeTurnId = entry.turnId;
        currentTurnId = entry.turnId;
        // Correlate the provisional user/request boundary (and any early
        // transient rows) with the backend-owned response root. This keeps the
        // logical turn contiguous even though the durable user row and root
        // response deliberately have different message IDs.
        for (const index of currentRequestFallbackIndices) {
          output[index] = { ...output[index], turnId: entry.turnId };
        }
      }
      output.push(entry);
      continue;
    }

    if (!currentTurnId) {
      output.push({ ...entry, turnId: undefined });
      continue;
    }

    currentRequestFallbackIndices.push(output.length);
    output.push({ ...entry, turnId: currentTurnId });
  }

  return output;
}

const getProcessState = (entry: TurnDisclosureInputItem): TurnDisclosureProcessState => {
  if (entry.processState) return entry.processState;
  if (entry.running) return 'running';
  return 'completed';
};

const getEffectiveProcessState = (
  entry: TurnDisclosureInputItem,
  options: { isClosed: boolean }
): TurnDisclosureProcessState => {
  const state = getProcessState(entry);
  if (options.isClosed && (state === 'running' || state === 'waiting')) {
    return 'completed';
  }
  return state;
};

const getProcessStartAt = (entry: TurnDisclosureInputItem): number => entry.processStartedAt ?? entry.createdAt;

const getProcessEndAt = (entry: TurnDisclosureInputItem): number => entry.processEndedAt ?? entry.createdAt;

const resolveDisclosureState = (
  processItems: TurnDisclosureInputItem[],
  options: { isClosed: boolean }
): TurnDisclosureProcessState => {
  const states = processItems.map((entry) => getEffectiveProcessState(entry, options));
  if (states.includes('waiting')) return 'waiting';
  if (states.includes('running')) return 'running';
  if (states.includes('failed')) return 'failed';
  if (states.includes('canceled')) return 'canceled';
  return 'completed';
};

const buildEmptyRunningDisclosure = (
  turnId: MessageId,
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

function buildSegmentOutput(
  segment: TurnDisclosureInputItem[],
  isClosed: boolean,
  finalAssistantForTurn?: TurnDisclosureInputItem
): TurnDisclosureOutputItem[] {
  const turnId = segment[0]?.turnId;
  if (!turnId) return segment.map((entry) => ({ type: 'item', id: entry.id }));

  // A delayed event from another turn can split one logical turn into several
  // segments. Only the last assistant row across the whole turn is final
  // answer content; earlier assistant rows remain part of the process trace.
  const finalAssistantIndex = finalAssistantForTurn
    ? segment.findIndex((entry) => entry === finalAssistantForTurn)
    : -1;
  const stateOptions = { isClosed };

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
  const terminalProcessState = getEffectiveProcessState(processItems.at(-1)!, stateOptions);
  // The header describes lifecycle, not whether every individual operation
  // succeeded. Once processing settles, every non-canceled turn is "processed";
  // intermediate failures remain available in `processItemStates` for the
  // expanded trace. While the turn is live, a failed step must not prematurely
  // close the header because the agent may recover and continue.
  const state: TurnDisclosureProcessState = isClosed
    ? terminalProcessState === 'canceled'
      ? 'canceled'
      : 'completed'
    : resolvedState === 'waiting'
      ? 'waiting'
      : 'running';

  const finalOrProcessItems = finalAssistantForTurn
    ? [...processItems, finalAssistantForTurn]
    : processItems;
  const disclosure: TurnDisclosureOutputItem = {
    type: 'turn_disclosure',
    id: `turn-disclosure-${turnId}`,
    turnId,
    processItemIds: processItems.map((entry) => entry.id),
    sourceMessageIds: unique(processItems.flatMap((entry) => entry.sourceMessageIds ?? [])),
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

const getDisclosureStateObservedAt = (
  disclosure: Extract<TurnDisclosureOutputItem, { type: 'turn_disclosure' }>,
  processObservedAtByItemId: ReadonlyMap<string, number>
): number => {
  if (!disclosure.processItemIds.length) return disclosure.endAt;
  const observedTimes = disclosure.processItemIds
    .map((itemId) => processObservedAtByItemId.get(itemId))
    .filter((value): value is number => value !== undefined);
  return observedTimes.length ? Math.max(...observedTimes) : disclosure.endAt;
};

const coalesceTurnDisclosures = (
  items: TurnDisclosureOutputItem[],
  processObservedAtByItemId: ReadonlyMap<string, number>
): TurnDisclosureOutputItem[] => {
  const output: TurnDisclosureOutputItem[] = [];
  const disclosureIndexByTurn = new Map<MessageId, number>();

  for (const item of items) {
    if (item.type !== 'turn_disclosure') {
      output.push(item);
      continue;
    }

    const existingIndex = disclosureIndexByTurn.get(item.turnId);
    if (existingIndex === undefined) {
      disclosureIndexByTurn.set(item.turnId, output.length);
      output.push(item);
      continue;
    }

    const existing = output[existingIndex];
    if (existing.type !== 'turn_disclosure') continue;
    // Header state describes the latest process observation, not the most
    // severe state ever seen. Duration endAt also includes the global final
    // answer, so it cannot safely decide freshness between split fragments.
    // On equal process timestamps, the later transcript fragment wins.
    const existingStateObservedAt = getDisclosureStateObservedAt(existing, processObservedAtByItemId);
    const itemStateObservedAt = getDisclosureStateObservedAt(item, processObservedAtByItemId);
    const state = itemStateObservedAt >= existingStateObservedAt ? item.state : existing.state;
    output[existingIndex] = {
      ...existing,
      processItemIds: unique([...existing.processItemIds, ...item.processItemIds]),
      sourceMessageIds: unique([...existing.sourceMessageIds, ...item.sourceMessageIds]),
      startAt: Math.min(existing.startAt, item.startAt),
      endAt: Math.max(existing.endAt, item.endAt),
      state,
      processItemStates: { ...existing.processItemStates, ...item.processItemStates },
      running: state === 'running' || state === 'waiting',
      defaultCollapsed: state !== 'running' && state !== 'waiting',
    };
  }

  return output;
};

export function buildTurnDisclosureItems(
  items: TurnDisclosureInputItem[],
  options: BuildTurnDisclosureOptions = {}
): TurnDisclosureOutputItem[] {
  const output: TurnDisclosureOutputItem[] = [];
  let segment: TurnDisclosureInputItem[] = [];
  const activeTurnId = options.tailClosed === true ? undefined : options.activeTurnId;
  const finalAssistantByTurn = new Map<MessageId, TurnDisclosureInputItem>();
  const processObservedAtByItemId = new Map<string, number>();

  for (const item of items) {
    if (item.turnId && item.role === 'assistant') {
      const currentFinal = finalAssistantByTurn.get(item.turnId);
      // Live rows are arrival-ordered and a delayed older text can be appended
      // after the real final answer. Choose by authoritative message time;
      // `>=` intentionally lets the later observation break timestamp ties.
      if (!currentFinal || item.createdAt >= currentFinal.createdAt) {
        finalAssistantByTurn.set(item.turnId, item);
      }
    }
    if (item.role !== 'user' && item.role !== 'other') {
      processObservedAtByItemId.set(item.id, getProcessEndAt(item));
    }
  }

  const flush = (fallbackClosed: boolean) => {
    if (!segment.length) return;
    const segmentTurnId = segment[0]?.turnId;
    const isClosed = options.tailClosed === true || (activeTurnId ? segmentTurnId !== activeTurnId : fallbackClosed);
    output.push(
      ...buildSegmentOutput(
        segment,
        isClosed,
        segmentTurnId ? finalAssistantByTurn.get(segmentTurnId) : undefined
      )
    );
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
  // Delayed events can make one logical turn appear in multiple non-contiguous
  // segments. Keep ordinary transcript items in arrival order, but fold their
  // synthetic process metadata into the first disclosure so IDs/DOM controls
  // remain unique and one turn can never render two "processed" headers.
  const coalesced = coalesceTurnDisclosures(output, processObservedAtByItemId);
  const liveDisclosureIndex = coalesced.findLastIndex(
    (item) => item.type === 'turn_disclosure' && item.running
  );
  if (liveDisclosureIndex < 0 || liveDisclosureIndex === coalesced.length - 1) return coalesced;

  // A live turn's process state is the transcript terminator. Keeping this
  // invariant after coalescing also covers text-only streaming and delayed
  // fragments from another turn without changing completed transcript order.
  const liveDisclosure = coalesced[liveDisclosureIndex];
  return [
    ...coalesced.slice(0, liveDisclosureIndex),
    ...coalesced.slice(liveDisclosureIndex + 1),
    liveDisclosure,
  ];
}
