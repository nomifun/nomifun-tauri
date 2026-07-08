/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { pickActiveQuestionIndex } from './ConversationQuestionLocator';

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
