/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { getDotDistanceLevel, pickActiveQuestionIndex } from './ConversationQuestionLocator';

describe('pickActiveQuestionIndex', () => {
  test('chooses the latest question above the viewport anchor', () => {
    expect(pickActiveQuestionIndex([-260, -24, 180, 420], 140)).toBe(1);
  });

  test('chooses the first question when every question is below the anchor', () => {
    expect(pickActiveQuestionIndex([180, 420], 140)).toBe(0);
  });

  test('returns -1 when no question anchors are available', () => {
    expect(pickActiveQuestionIndex([], 140)).toBe(-1);
  });
});

describe('getDotDistanceLevel', () => {
  test('marks the active question as level 0', () => {
    expect(getDotDistanceLevel(3, 3)).toBe(0);
  });

  test('marks direct neighbors as level 1', () => {
    expect(getDotDistanceLevel(2, 3)).toBe(1);
    expect(getDotDistanceLevel(4, 3)).toBe(1);
  });

  test('marks second neighbors as level 2 and farther dots as level 3', () => {
    expect(getDotDistanceLevel(1, 3)).toBe(2);
    expect(getDotDistanceLevel(5, 3)).toBe(2);
    expect(getDotDistanceLevel(0, 3)).toBe(3);
    expect(getDotDistanceLevel(8, 3)).toBe(3);
  });
});
