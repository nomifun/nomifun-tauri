/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { buildModelFailoverConfigForSave } from './modelFailoverQueue';
import { parseProviderId } from '@/common/types/ids';

const P1 = parseProviderId('019b0000-0000-7000-8000-000000000001');
const P2 = parseProviderId('019b0000-0000-7000-8000-000000000002');

const baseConfig = {
  enabled: true,
  queue: [{ provider_id: P1, model: 'm1' }],
  max_switches: 4,
  stamp_unhealthy: true,
};

describe('buildModelFailoverConfigForSave', () => {
  test('includes the complete draft provider and model when saving', () => {
    const result = buildModelFailoverConfigForSave(baseConfig, P2, 'm2');

    expect(result.config.queue).toEqual([
      { provider_id: P1, model: 'm1' },
      { provider_id: P2, model: 'm2' },
    ]);
    expect(result.appendedDraft).toBe(true);
  });

  test('does not duplicate a candidate that is already in the queue', () => {
    const result = buildModelFailoverConfigForSave(baseConfig, P1, 'm1');

    expect(result.config.queue).toEqual([{ provider_id: P1, model: 'm1' }]);
    expect(result.appendedDraft).toBe(false);
  });

  test('ignores incomplete draft selections', () => {
    const result = buildModelFailoverConfigForSave(baseConfig, P2, undefined);

    expect(result.config.queue).toEqual([{ provider_id: P1, model: 'm1' }]);
    expect(result.appendedDraft).toBe(false);
  });
});
