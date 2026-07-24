/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { parseConversationId } from '@/common/types/ids';

import { processLocalCronResponse } from './localCronCommands';

describe('processLocalCronResponse', () => {
  test('does not replace a non-empty assistant message with an empty display string', async () => {
    const result = await processLocalCronResponse(parseConversationId('0190f5fe-7c00-7a00-8000-000000000005'), '<think>The answer is being prepared.</think>');

    expect(result.displayContent).toBeUndefined();
    expect(result.systemResponses).toEqual([]);
  });

  test('still strips think tags when visible answer content remains', async () => {
    const result = await processLocalCronResponse(parseConversationId('0190f5fe-7c00-7a00-8000-000000000005'), '<think>Scratch work</think>\n\nFinal answer');

    expect(result.displayContent).toBe('Final answer');
    expect(result.systemResponses).toEqual([]);
  });
});
