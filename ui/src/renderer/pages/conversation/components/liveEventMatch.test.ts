/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';

import { isLiveEventForTarget } from './liveEventMatch';

describe('isLiveEventForTarget', () => {
  test('matches when the event target_id (string, as the backend sends it) equals a numeric control id', () => {
    // REGRESSION (header ↔ sidebar desync): the per-session header control froze
    // because `s.target_id === id` was `"37" === 37` (string vs number) and never
    // matched, so live status events were dropped. Coerced compare must match.
    expect(isLiveEventForTarget('conversation', '37', 'conversation', 37)).toBe(true);
  });

  test('documents the old strict-=== bug this fixes', () => {
    expect(('37' as unknown) === (37 as unknown)).toBe(false);
  });

  test('does not match a different target id', () => {
    expect(isLiveEventForTarget('conversation', '38', 'conversation', 37)).toBe(false);
  });

  test('does not match a different kind even when ids collide (conversation vs terminal)', () => {
    expect(isLiveEventForTarget('terminal', '37', 'conversation', 37)).toBe(false);
  });

  test('matches when both sides are strings', () => {
    expect(isLiveEventForTarget('conversation', '37', 'conversation', '37')).toBe(true);
  });
});
