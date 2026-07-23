/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  assignTurnIdsFromUserRequests,
  buildTurnDisclosureItems,
  type TurnDisclosureInputItem,
} from './turnDisclosureModel';
import { parseMessageId } from '@/common/types/ids';

const TURN_1 = parseMessageId('0190f5fe-7c00-7a00-8000-000000000001');
const TURN_2 = parseMessageId('0190f5fe-7c00-7a00-8000-000000000002');
const ACP_ROOT_1 = parseMessageId('0190f5fe-7c00-7a00-8000-000000000011');
const SOURCE_1 = parseMessageId('0190f5fe-7c00-7a00-8000-000000000021');
const SOURCE_2 = parseMessageId('0190f5fe-7c00-7a00-8000-000000000022');
const DISCLOSURE_1 = `turn-disclosure-${TURN_1}`;
const DISCLOSURE_2 = `turn-disclosure-${TURN_2}`;

const item = (
  id: string,
  role: TurnDisclosureInputItem['role'],
  options: Partial<TurnDisclosureInputItem> = {}
): TurnDisclosureInputItem => ({
  id,
  turnId: TURN_1,
  role,
  createdAt: options.createdAt ?? 1000,
  sourceMessageIds: options.sourceMessageIds ?? [],
  ...options,
});

describe('buildTurnDisclosureItems', () => {
  test('collapses completed intermediate steps into a disclosure before the final answer', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('analysis', 'process', { createdAt: 2000, sourceMessageIds: [SOURCE_1] }),
        item('tool', 'process', { createdAt: 3000, sourceMessageIds: [SOURCE_2] }),
        item('final', 'assistant', { createdAt: 5000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => entry.type === 'item' ? entry.id : entry.id)).toEqual([
      'user',
      DISCLOSURE_1,
      'final',
    ]);

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.defaultCollapsed).toBe(true);
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemIds).toEqual(['analysis', 'tool']);
    expect(disclosure.startAt).toBe(2000);
    expect(disclosure.endAt).toBe(5000);
    expect(disclosure.sourceMessageIds).toEqual([SOURCE_1, SOURCE_2]);
  });

  test('uses completed process intervals when calculating disclosure duration', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 0 }),
        item('analysis', 'process', {
          createdAt: 35000,
          processStartedAt: 1000,
          processEndedAt: 35000,
        }),
        item('tool', 'process', { createdAt: 33000 }),
        item('final', 'assistant', { createdAt: 35600 }),
      ],
      { tailClosed: true }
    );

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.processItemIds).toEqual(['analysis', 'tool']);
    expect(disclosure.startAt).toBe(1000);
    expect(disclosure.endAt).toBe(35600);
  });

  test('keeps the final assistant answer outside the disclosure when earlier assistant text was intermediate', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('analysis-note', 'assistant', { createdAt: 1500 }),
        item('tool', 'process', { createdAt: 2000 }),
        item('summary', 'assistant', { createdAt: 4000 }),
      ],
      { tailClosed: true }
    );

    const disclosure = result.find((entry) => entry.type === 'turn_disclosure');
    expect(disclosure?.processItemIds).toEqual(['analysis-note', 'tool']);
    expect(result.map((entry) => entry.type === 'item' ? entry.id : entry.id)).toEqual([
      'user',
      DISCLOSURE_1,
      'summary',
    ]);
  });

  test('renders unfinished running process steps as a live turn disclosure before the final answer exists', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('analysis', 'process', { createdAt: 2000, processStartedAt: 1500, processState: 'running' }),
      item('tool', 'process', { createdAt: 3000, processEndedAt: 3200 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      DISCLOSURE_1,
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.running).toBe(true);
    expect(disclosure.defaultCollapsed).toBe(false);
    expect(disclosure.processItemIds).toEqual(['analysis', 'tool']);
    expect(disclosure.startAt).toBe(1500);
    expect(disclosure.endAt).toBe(3200);
  });

  test('keeps a live disclosure visible while the current turn waits for the first process item', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      DISCLOSURE_1,
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.running).toBe(true);
    expect(disclosure.defaultCollapsed).toBe(false);
    expect(disclosure.processItemIds).toEqual([]);
    expect(disclosure.sourceMessageIds).toEqual([]);
    expect(disclosure.startAt).toBe(1000);
    expect(disclosure.endAt).toBe(1000);
  });

  test('keeps a text-only streaming turn terminated by its live disclosure', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('streaming-text', 'assistant', { createdAt: 2000 }),
    ]);

    expect(result.map((entry) => entry.id)).toEqual(['user', 'streaming-text', DISCLOSURE_1]);
    expect(result.at(-1)?.type).toBe('turn_disclosure');
  });

  test('keeps the current turn disclosure visible between active process phases', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'completed' }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      DISCLOSURE_1,
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.running).toBe(true);
    expect(disclosure.defaultCollapsed).toBe(false);
    expect(disclosure.processItemIds).toEqual(['tool']);
    expect(disclosure.processItemStates).toEqual({ tool: 'completed' });
  });

  test('keeps thinking items inside the process disclosure content', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('thinking', 'process_content', { createdAt: 1500, processState: 'running' }),
      item('tool', 'process', { createdAt: 2000, processState: 'running' }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      DISCLOSURE_1,
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.processItemIds).toEqual(['thinking', 'tool']);
  });

  test('does not archive an empty disclosure when a turn closes without process items', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
      ],
      { tailClosed: true }
    );

    expect(result).toEqual([{ type: 'item', id: 'user' }]);
  });

  test('collapses stale running process steps after a closed turn has a final answer', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('tool', 'process', { createdAt: 2000, processState: 'running' }),
        item('final', 'assistant', { createdAt: 3000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      DISCLOSURE_1,
      'final',
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemStates).toEqual({ tool: 'completed' });
  });

  test('settles stale running thinking when a process-only turn closes', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('thinking', 'process_content', { createdAt: 2000, processState: 'running' }),
      ],
      { tailClosed: true }
    );

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('completed');
    expect(disclosure.running).toBe(false);
    expect(disclosure.processItemStates).toEqual({ thinking: 'completed' });
  });

  test('keeps the live disclosure after running assistant text', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('progress-note', 'assistant', { createdAt: 1500 }),
      item('scan', 'process', { createdAt: 2000, processState: 'running' }),
      item('partial-answer', 'assistant', { createdAt: 3000 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'partial-answer',
      DISCLOSURE_1,
    ]);
    const disclosure = result[2];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.processItemIds).toEqual(['progress-note', 'scan']);
  });

  test('keeps waiting confirmation steps visible in the live disclosure', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('permission', 'process', { createdAt: 2000, processState: 'waiting' }),
      item('partial-answer', 'assistant', { createdAt: 3000 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'partial-answer',
      DISCLOSURE_1,
    ]);
    const disclosure = result[2];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('waiting');
    expect(disclosure.running).toBe(true);
    expect(disclosure.defaultCollapsed).toBe(false);
    expect(disclosure.processItemIds).toEqual(['permission']);
  });

  test('keeps an intermediate failure in details but marks a closed answered turn as processed', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('tool', 'process', { createdAt: 2000, processState: 'failed' }),
        item('final', 'assistant', { createdAt: 3000 }),
      ],
      { tailClosed: true }
    );

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.defaultCollapsed).toBe(true);
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemStates).toEqual({ tool: 'failed' });
  });

  test('marks a closed failed process-only turn as processed while retaining failed details', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('tool', 'process', {
          createdAt: 3000,
          processStartedAt: 1500,
          processEndedAt: 3000,
          processState: 'failed',
        }),
      ],
      { tailClosed: true }
    );

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('completed');
    expect(disclosure.running).toBe(false);
    expect(disclosure.startAt).toBe(1500);
    expect(disclosure.endAt).toBe(3000);
    expect(disclosure.processItemStates).toEqual({ tool: 'failed' });
  });

  test('keeps an in-flight turn processing after an intermediate failure', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'failed' }),
    ]);

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.running).toBe(true);
    expect(disclosure.processItemStates).toEqual({ tool: 'failed' });
  });

  test('keeps a canceled closed turn and its execution interval', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('tool', 'process', {
          createdAt: 5200,
          processStartedAt: 1200,
          processEndedAt: 5200,
          processState: 'canceled',
        }),
      ],
      { tailClosed: true }
    );

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('canceled');
    expect(disclosure.running).toBe(false);
    expect(disclosure.startAt).toBe(1200);
    expect(disclosure.endAt).toBe(5200);
    expect(disclosure.processItemStates).toEqual({ tool: 'canceled' });
  });

  test('lets a final cancellation override an earlier failed process item', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('failed-tool', 'process', {
          createdAt: 2800,
          processStartedAt: 1200,
          processEndedAt: 2800,
          processState: 'failed',
        }),
        item('canceled-tool', 'process', {
          createdAt: 6200,
          processStartedAt: 3000,
          processEndedAt: 6200,
          processState: 'canceled',
        }),
      ],
      { tailClosed: true }
    );

    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('canceled');
    expect(disclosure.startAt).toBe(1200);
    expect(disclosure.endAt).toBe(6200);
    expect(disclosure.processItemStates).toEqual({
      'failed-tool': 'failed',
      'canceled-tool': 'canceled',
    });
  });

  test('keeps a completed process-only tail inside the live disclosure until the request closes', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'completed' }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      DISCLOSURE_1,
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.processItemIds).toEqual(['tool']);
  });

  test('keeps a completed live disclosure after assistant text so processing remains the tail', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'completed' }),
      item('assistant-text', 'assistant', { createdAt: 3000 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'assistant-text',
      DISCLOSURE_1,
    ]);
    const disclosure = result[2];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.processItemIds).toEqual(['tool']);
  });

  test('collapses a completed process-only segment once the next user request closes it', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000, processState: 'completed' }),
        item('user-2', 'user', { turnId: TURN_2, createdAt: 3000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user-1',
      DISCLOSURE_1,
      'user-2',
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.defaultCollapsed).toBe(true);
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemIds).toEqual(['tool-1']);
  });

  test('keeps completed disclosures scoped to their own turn id', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000 }),
        item('final-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
        item('user-2', 'user', { turnId: TURN_2, createdAt: 4000 }),
        item('tool-2', 'process', { turnId: TURN_2, createdAt: 5000 }),
        item('final-2', 'assistant', { turnId: TURN_2, createdAt: 6000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user-1',
      DISCLOSURE_1,
      'final-1',
      'user-2',
      DISCLOSURE_2,
      'final-2',
    ]);
    expect(result.filter((entry) => entry.type === 'turn_disclosure')).toHaveLength(2);
  });

  test('folds a delayed process fragment into the first disclosure without reordering visible messages', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000 }),
        item('final-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
        item('user-2', 'user', { turnId: TURN_2, createdAt: 4000 }),
        item('tool-2', 'process', { turnId: TURN_2, createdAt: 5000 }),
        item('final-2', 'assistant', { turnId: TURN_2, createdAt: 6000 }),
        item('late-tool-1', 'process', { turnId: TURN_1, createdAt: 7000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => entry.id)).toEqual([
      'user-1',
      DISCLOSURE_1,
      'final-1',
      'user-2',
      DISCLOSURE_2,
      'final-2',
    ]);
    expect(new Set(result.map((entry) => entry.id)).size).toBe(result.length);
    const firstDisclosure = result.find(
      (entry) => entry.type === 'turn_disclosure' && entry.turnId === TURN_1
    );
    expect(firstDisclosure?.type).toBe('turn_disclosure');
    if (firstDisclosure?.type !== 'turn_disclosure') return;
    expect(firstDisclosure.processItemIds).toEqual(['tool-1', 'late-tool-1']);
    expect(firstDisclosure.endAt).toBe(7000);
  });

  test('selects final assistant content across non-contiguous fragments of the same turn', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('intro-1', 'assistant', { turnId: TURN_1, createdAt: 1500 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000 }),
        item('delayed-turn-2', 'process', { turnId: TURN_2, createdAt: 2500 }),
        item('final-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => entry.id)).toEqual([
      'user-1',
      DISCLOSURE_1,
      DISCLOSURE_2,
      'final-1',
    ]);
    const disclosure = result.find(
      (entry) => entry.type === 'turn_disclosure' && entry.turnId === TURN_1
    );
    expect(disclosure?.type).toBe('turn_disclosure');
    if (disclosure?.type !== 'turn_disclosure') return;
    expect(disclosure.processItemIds).toEqual(['intro-1', 'tool-1']);
    expect(disclosure.endAt).toBe(3000);
  });

  test('uses the latest fragment state when a canceled turn later completes', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('canceled-1', 'process', {
          turnId: TURN_1,
          createdAt: 2000,
          processState: 'canceled',
        }),
        item('delayed-turn-2', 'process', { turnId: TURN_2, createdAt: 2500 }),
        item('completed-1', 'process', {
          turnId: TURN_1,
          createdAt: 3000,
          processState: 'completed',
        }),
        item('final-1', 'assistant', { turnId: TURN_1, createdAt: 3500 }),
      ],
      { tailClosed: true }
    );

    const disclosure = result.find(
      (entry) => entry.type === 'turn_disclosure' && entry.turnId === TURN_1
    );
    expect(disclosure?.type).toBe('turn_disclosure');
    if (disclosure?.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemStates).toEqual({
      'canceled-1': 'canceled',
      'completed-1': 'completed',
    });
  });

  test('uses the latest fragment state when a waiting turn resumes running', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('permission-1', 'process', {
          turnId: TURN_1,
          createdAt: 2000,
          processState: 'waiting',
        }),
        item('delayed-turn-2', 'process', { turnId: TURN_2, createdAt: 2500 }),
        item('running-1', 'process', {
          turnId: TURN_1,
          createdAt: 3000,
          processState: 'running',
        }),
      ],
      { activeTurnId: TURN_1 }
    );

    const disclosure = result.find(
      (entry) => entry.type === 'turn_disclosure' && entry.turnId === TURN_1
    );
    expect(disclosure?.type).toBe('turn_disclosure');
    if (disclosure?.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.running).toBe(true);
  });

  test('does not let an older process state win merely because every fragment includes the global final time', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('completed-1', 'process', {
          turnId: TURN_1,
          createdAt: 5000,
          processState: 'completed',
        }),
        item('final-1', 'assistant', { turnId: TURN_1, createdAt: 6000 }),
        item('delayed-turn-2', 'process', { turnId: TURN_2, createdAt: 6500 }),
        item('stale-canceled-1', 'process', {
          turnId: TURN_1,
          createdAt: 2000,
          processState: 'canceled',
        }),
      ],
      { tailClosed: true }
    );

    const disclosure = result.find(
      (entry) => entry.type === 'turn_disclosure' && entry.turnId === TURN_1
    );
    expect(disclosure?.type).toBe('turn_disclosure');
    if (disclosure?.type !== 'turn_disclosure') return;
    expect(disclosure.endAt).toBe(6000);
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemStates['stale-canceled-1']).toBe('canceled');
  });

  test('does not mistake a delayed older assistant row for the final answer merely because it was appended last', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000 }),
        item('true-final-1', 'assistant', { turnId: TURN_1, createdAt: 6000 }),
        item('delayed-turn-2', 'process', { turnId: TURN_2, createdAt: 6500 }),
        item('late-old-intro-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => entry.id)).toEqual([
      'user-1',
      DISCLOSURE_1,
      'true-final-1',
      DISCLOSURE_2,
    ]);
    const disclosure = result.find(
      (entry) => entry.type === 'turn_disclosure' && entry.turnId === TURN_1
    );
    expect(disclosure?.type).toBe('turn_disclosure');
    if (disclosure?.type !== 'turn_disclosure') return;
    expect(disclosure.processItemIds).toEqual(['tool-1', 'late-old-intro-1']);
    expect(disclosure.endAt).toBe(6000);
  });

  test('keeps the latest user turn running when an older turn fragment arrives last', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000 }),
        item('final-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
        item('user-2', 'user', { turnId: TURN_2, createdAt: 4000 }),
        item('tool-2', 'process', { turnId: TURN_2, createdAt: 5000, processState: 'running' }),
        item('late-tool-1', 'process', { turnId: TURN_1, createdAt: 6000 }),
      ],
      { activeTurnId: TURN_2 }
    );

    const disclosures = result.filter((entry) => entry.type === 'turn_disclosure');
    expect(disclosures).toHaveLength(2);
    expect(disclosures.find((entry) => entry.turnId === TURN_1)?.state).toBe('completed');
    expect(disclosures.find((entry) => entry.turnId === TURN_2)?.state).toBe('running');
  });

  test('keeps a coalesced active-turn disclosure at the transcript tail', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('tool-1', 'process', { turnId: TURN_1, createdAt: 2000, processState: 'completed' }),
        item('partial-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
        item('delayed-tool-2', 'process', { turnId: TURN_2, createdAt: 4000, processState: 'completed' }),
        item('running-1', 'process', { turnId: TURN_1, createdAt: 5000, processState: 'running' }),
      ],
      { activeTurnId: TURN_1 }
    );

    expect(result.at(-1)?.type).toBe('turn_disclosure');
    expect(result.at(-1)?.id).toBe(DISCLOSURE_1);
    expect(result.filter((entry) => entry.id === DISCLOSURE_1)).toHaveLength(1);
  });

  test('uses an explicit active turn for background work without a visible user row', () => {
    const result = buildTurnDisclosureItems(
      [
        item('current-background-tool', 'process', {
          turnId: TURN_2,
          createdAt: 1000,
          processState: 'running',
        }),
        item('delayed-old-tool', 'process', { turnId: TURN_1, createdAt: 2000 }),
      ],
      { activeTurnId: TURN_2 }
    );

    const disclosures = result.filter((entry) => entry.type === 'turn_disclosure');
    expect(disclosures.find((entry) => entry.turnId === TURN_2)?.state).toBe('running');
    expect(disclosures.find((entry) => entry.turnId === TURN_1)?.state).toBe('completed');
  });

  test('renders process steps without a visible user request as inline receipts', () => {
    const result = buildTurnDisclosureItems([
      item('scan', 'process', { turnId: undefined, createdAt: 1000, processState: 'completed' }),
      item('tool', 'process', { turnId: undefined, createdAt: 1500, processState: 'completed' }),
      item('assistant-text', 'assistant', { turnId: undefined, createdAt: 2000 }),
    ]);

    expect(result).toEqual([
      { type: 'process_receipt', id: 'receipt-scan', itemId: 'scan' },
      { type: 'process_receipt', id: 'receipt-tool', itemId: 'tool' },
      { type: 'item', id: 'assistant-text' },
    ]);
  });
});

describe('assignTurnIdsFromUserRequests', () => {
  test('promotes the ACP root turn id over the distinct user request id for unowned tail rows', () => {
    const assigned = assignTurnIdsFromUserRequests([
      item('user', 'user', { turnId: TURN_1, createdAt: 1000 }),
      item('tool', 'process', { turnId: ACP_ROOT_1, createdAt: 2000, processState: 'completed' }),
      item('final', 'assistant', { turnId: ACP_ROOT_1, createdAt: 3000 }),
      item('terminal-orphan', 'process', {
        turnId: undefined,
        createdAt: 3001,
        processState: 'completed',
      }),
    ]);

    expect(assigned.map((entry) => entry.turnId)).toEqual([
      ACP_ROOT_1,
      ACP_ROOT_1,
      ACP_ROOT_1,
      ACP_ROOT_1,
    ]);

    const display = buildTurnDisclosureItems(assigned, { tailClosed: true });
    const disclosures = display.filter((entry) => entry.type === 'turn_disclosure');
    expect(disclosures).toHaveLength(1);
    expect(disclosures[0]?.turnId).toBe(ACP_ROOT_1);
    expect(disclosures[0]?.processItemIds).toEqual(['tool', 'terminal-orphan']);
    expect(display.map((entry) => entry.id)).toEqual([
      'user',
      `turn-disclosure-${ACP_ROOT_1}`,
      'final',
    ]);
    expect(new Set(display.map((entry) => entry.id)).size).toBe(display.length);
  });

  test('groups all assistant and process messages after one user request into the same turn', () => {
    const result = assignTurnIdsFromUserRequests([
      item('user', 'user', { turnId: TURN_1, createdAt: 1000 }),
      item('scan', 'process', { turnId: undefined, createdAt: 1500 }),
      item('progress', 'assistant', { turnId: undefined, createdAt: 2000 }),
      item('tool', 'process', { turnId: undefined, createdAt: 2500 }),
      item('final', 'assistant', { turnId: undefined, createdAt: 3000 }),
    ]);

    expect(result.map((entry) => entry.turnId)).toEqual([TURN_1, TURN_1, TURN_1, TURN_1, TURN_1]);
  });

  test('starts a new request group at the next user message and leaves leading system items ungrouped', () => {
    const result = assignTurnIdsFromUserRequests([
      item('status', 'other', { turnId: undefined, createdAt: 500 }),
      item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
      item('tool-1', 'process', { turnId: undefined, createdAt: 1500 }),
      item('user-2', 'user', { turnId: TURN_2, createdAt: 2000 }),
      item('tool-2', 'process', { turnId: undefined, createdAt: 2500 }),
    ]);

    expect(result.map((entry) => entry.turnId)).toEqual([undefined, TURN_1, TURN_1, TURN_2, TURN_2]);
  });

  test('preserves an authoritative delayed turn id without poisoning the positional fallback', () => {
    const result = assignTurnIdsFromUserRequests([
      item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
      item('user-2', 'user', { turnId: TURN_2, createdAt: 2000 }),
      item('delayed-error-1', 'assistant', { turnId: TURN_1, createdAt: 3000 }),
      item('legacy-tool-2', 'process', { turnId: undefined, createdAt: 4000 }),
    ]);

    expect(result.map((entry) => entry.turnId)).toEqual([TURN_1, TURN_2, TURN_1, TURN_2]);
  });

  test('does not promote a retired provisional request id before the current root arrives', () => {
    const userTurn2 = parseMessageId('0190f5fe-7c00-7a00-8000-000000000022');
    const result = assignTurnIdsFromUserRequests([
      item('user-1', 'user', { turnId: TURN_1, createdAt: 1000 }),
      item('root-1', 'process', { turnId: ACP_ROOT_1, createdAt: 1500 }),
      item('user-2', 'user', { turnId: userTurn2, createdAt: 2000 }),
      item('delayed-provisional-1', 'assistant', { turnId: TURN_1, createdAt: 2500 }),
      item('legacy-tool-2', 'process', { turnId: undefined, createdAt: 3000 }),
    ]);

    expect(result.map((entry) => entry.turnId)).toEqual([
      ACP_ROOT_1,
      ACP_ROOT_1,
      userTurn2,
      TURN_1,
      userTurn2,
    ]);
  });

  test('uses the active request correlation before the first root-owned stream row arrives', () => {
    const assigned = assignTurnIdsFromUserRequests(
      [
        item('user', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('early-thinking', 'process_content', {
          turnId: undefined,
          createdAt: 1200,
          processState: 'running',
        }),
      ],
      { activeRequestMessageId: TURN_1, activeTurnId: ACP_ROOT_1 }
    );

    expect(assigned.map((entry) => entry.turnId)).toEqual([ACP_ROOT_1, ACP_ROOT_1]);
    const display = buildTurnDisclosureItems(assigned, { activeTurnId: ACP_ROOT_1 });
    expect(display.map((entry) => entry.id)).toEqual([
      'user',
      `turn-disclosure-${ACP_ROOT_1}`,
    ]);
    expect(display[1]?.type).toBe('turn_disclosure');
    if (display[1]?.type !== 'turn_disclosure') return;
    expect(display[1].state).toBe('running');
    expect(display[1].processItemIds).toEqual(['early-thinking']);
  });

  test('does not let an unseen delayed old turn override an authoritative active request pair', () => {
    const assigned = assignTurnIdsFromUserRequests(
      [
        item('user', 'user', { turnId: TURN_1, createdAt: 1000 }),
        item('delayed-old-event', 'process', { turnId: TURN_2, createdAt: 1100 }),
        item('current-unowned-thinking', 'process_content', {
          turnId: undefined,
          createdAt: 1200,
          processState: 'running',
        }),
      ],
      { activeRequestMessageId: TURN_1, activeTurnId: ACP_ROOT_1 }
    );

    expect(assigned.map((entry) => entry.turnId)).toEqual([ACP_ROOT_1, TURN_2, ACP_ROOT_1]);
  });
});
