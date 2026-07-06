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

const item = (
  id: string,
  role: TurnDisclosureInputItem['role'],
  options: Partial<TurnDisclosureInputItem> = {}
): TurnDisclosureInputItem => ({
  id,
  turnId: 'turn-1',
  role,
  createdAt: options.createdAt ?? 1000,
  sourceMessageIds: options.sourceMessageIds ?? [id],
  ...options,
});

describe('buildTurnDisclosureItems', () => {
  test('collapses completed intermediate steps into a disclosure before the final answer', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user', 'user', { createdAt: 1000 }),
        item('analysis', 'process', { createdAt: 2000 }),
        item('tool', 'process', { createdAt: 3000 }),
        item('final', 'assistant', { createdAt: 5000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => entry.type === 'item' ? entry.id : entry.id)).toEqual([
      'user',
      'turn-disclosure-turn-1',
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
    expect(disclosure.sourceMessageIds).toEqual(['analysis', 'tool']);
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
      'turn-disclosure-turn-1',
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
      'turn-disclosure-turn-1',
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
      'turn-disclosure-turn-1',
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

  test('keeps the current turn disclosure visible between active process phases', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'completed' }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'turn-disclosure-turn-1',
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
      'turn-disclosure-turn-1',
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
      'turn-disclosure-turn-1',
      'final',
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('completed');
    expect(disclosure.processItemStates).toEqual({ tool: 'completed' });
  });

  test('keeps running assistant text visible after the live disclosure', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('progress-note', 'assistant', { createdAt: 1500 }),
      item('scan', 'process', { createdAt: 2000, processState: 'running' }),
      item('partial-answer', 'assistant', { createdAt: 3000 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'turn-disclosure-turn-1',
      'partial-answer',
    ]);
    const disclosure = result[1];
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
      'turn-disclosure-turn-1',
      'partial-answer',
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('waiting');
    expect(disclosure.running).toBe(true);
    expect(disclosure.defaultCollapsed).toBe(false);
    expect(disclosure.processItemIds).toEqual(['permission']);
  });

  test('surfaces failed process state on a completed disclosure', () => {
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
    expect(disclosure.state).toBe('failed');
  });

  test('keeps a completed process-only tail inside the live disclosure until the request closes', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'completed' }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'turn-disclosure-turn-1',
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.processItemIds).toEqual(['tool']);
  });

  test('keeps a completed tail in the live disclosure while assistant text remains readable', () => {
    const result = buildTurnDisclosureItems([
      item('user', 'user', { createdAt: 1000 }),
      item('tool', 'process', { createdAt: 2000, processState: 'completed' }),
      item('assistant-text', 'assistant', { createdAt: 3000 }),
    ]);

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user',
      'turn-disclosure-turn-1',
      'assistant-text',
    ]);
    const disclosure = result[1];
    expect(disclosure.type).toBe('turn_disclosure');
    if (disclosure.type !== 'turn_disclosure') return;
    expect(disclosure.state).toBe('running');
    expect(disclosure.processItemIds).toEqual(['tool']);
  });

  test('collapses a completed process-only segment once the next user request closes it', () => {
    const result = buildTurnDisclosureItems(
      [
        item('user-1', 'user', { turnId: 'turn-1', createdAt: 1000 }),
        item('tool-1', 'process', { turnId: 'turn-1', createdAt: 2000, processState: 'completed' }),
        item('user-2', 'user', { turnId: 'turn-2', createdAt: 3000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user-1',
      'turn-disclosure-turn-1',
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
        item('user-1', 'user', { turnId: 'turn-1', createdAt: 1000 }),
        item('tool-1', 'process', { turnId: 'turn-1', createdAt: 2000 }),
        item('final-1', 'assistant', { turnId: 'turn-1', createdAt: 3000 }),
        item('user-2', 'user', { turnId: 'turn-2', createdAt: 4000 }),
        item('tool-2', 'process', { turnId: 'turn-2', createdAt: 5000 }),
        item('final-2', 'assistant', { turnId: 'turn-2', createdAt: 6000 }),
      ],
      { tailClosed: true }
    );

    expect(result.map((entry) => (entry.type === 'item' ? entry.id : entry.id))).toEqual([
      'user-1',
      'turn-disclosure-turn-1',
      'final-1',
      'user-2',
      'turn-disclosure-turn-2',
      'final-2',
    ]);
    expect(result.filter((entry) => entry.type === 'turn_disclosure')).toHaveLength(2);
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
  test('groups all assistant and process messages after one user request into the same turn', () => {
    const result = assignTurnIdsFromUserRequests([
      item('user', 'user', { turnId: undefined, createdAt: 1000 }),
      item('scan', 'process', { turnId: undefined, createdAt: 1500 }),
      item('progress', 'assistant', { turnId: undefined, createdAt: 2000 }),
      item('tool', 'process', { turnId: undefined, createdAt: 2500 }),
      item('final', 'assistant', { turnId: undefined, createdAt: 3000 }),
    ]);

    expect(result.map((entry) => entry.turnId)).toEqual(['user', 'user', 'user', 'user', 'user']);
  });

  test('starts a new request group at the next user message and leaves leading system items ungrouped', () => {
    const result = assignTurnIdsFromUserRequests([
      item('status', 'other', { turnId: undefined, createdAt: 500 }),
      item('user-1', 'user', { turnId: undefined, createdAt: 1000 }),
      item('tool-1', 'process', { turnId: undefined, createdAt: 1500 }),
      item('user-2', 'user', { turnId: undefined, createdAt: 2000 }),
      item('tool-2', 'process', { turnId: undefined, createdAt: 2500 }),
    ]);

    expect(result.map((entry) => entry.turnId)).toEqual([undefined, 'user-1', 'user-1', 'user-2', 'user-2']);
  });
});
