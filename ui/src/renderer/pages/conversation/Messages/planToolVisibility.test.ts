/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { isSupersededPlanToolFailure } from './planToolVisibility';

describe('isSupersededPlanToolFailure', () => {
  test('hides only the historical synthetic update_plan failure when a plan projection exists', () => {
    const failedTool = {
      type: 'tool_call',
      created_at: 100,
      content: {
        call_id: 'call-1',
        name: 'update_plan',
        status: 'error',
        output: 'The turn ended before this tool completed: error',
      },
    } as any;
    const plan = {
      type: 'plan',
      created_at: 101,
      content: { session_id: 'update_plan', entries: [] },
    } as any;

    expect(isSupersededPlanToolFailure(failedTool, [plan])).toBe(true);
    expect(
      isSupersededPlanToolFailure(
        { ...failedTool, content: { ...failedTool.content, output: 'invalid plan input' } },
        [plan]
      )
    ).toBe(false);
    expect(isSupersededPlanToolFailure(failedTool, [])).toBe(false);
  });
});
