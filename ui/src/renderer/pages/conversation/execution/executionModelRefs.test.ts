import { describe, expect, test } from 'bun:test';

import type { TExecutionModelRef } from '@/common/types/agentExecution/agentExecutionTypes';
import { parseProviderId, type ProviderId } from '@/common/types/ids';
import { reconcileModelRefs, sameModelRefs } from './executionModelRefs';

const GONE = parseProviderId('0190f5fe-7c00-7a00-8000-000000000001');
const KEEP = parseProviderId('0190f5fe-7c00-7a00-8000-000000000002');
const P = parseProviderId('0190f5fe-7c00-7a00-8000-000000000003');

const ref = (provider_id: ProviderId, model: string): TExecutionModelRef => ({
  provider_id,
  model,
});

describe('collaboration model reference reconciliation', () => {
  test('removes a deleted provider while preserving valid order', () => {
    const result = reconcileModelRefs(
      [ref(GONE, 'g1'), ref(KEEP, 'k2'), ref(KEEP, 'k1')],
      [ref(KEEP, 'k1'), ref(KEEP, 'k2')],
      [ref(KEEP, 'k1'), ref(KEEP, 'k2')],
    );

    expect(result.retained).toEqual([ref(KEEP, 'k2'), ref(KEEP, 'k1')]);
    expect(result.active).toEqual(result.retained);
    expect(result.removed).toEqual([ref(GONE, 'g1')]);
  });

  test('removes a deleted model from a surviving provider', () => {
    const result = reconcileModelRefs(
      [ref(KEEP, 'removed-model'), ref(KEEP, 'live-model')],
      [ref(KEEP, 'live-model')],
      [ref(KEEP, 'live-model')],
    );

    expect(result.retained).toEqual([ref(KEEP, 'live-model')]);
    expect(result.active).toEqual([ref(KEEP, 'live-model')]);
    expect(result.removed).toEqual([ref(KEEP, 'removed-model')]);
  });

  test('retains but deactivates disabled models', () => {
    const result = reconcileModelRefs(
      [ref(P, 'disabled'), ref(P, 'enabled')],
      [ref(P, 'disabled'), ref(P, 'enabled')],
      [ref(P, 'enabled')],
    );

    expect(result.retained).toEqual([ref(P, 'disabled'), ref(P, 'enabled')]);
    expect(result.active).toEqual([ref(P, 'enabled')]);
    expect(result.removed).toEqual([]);
  });

  test('deduplicates references while preserving the first occurrence', () => {
    const result = reconcileModelRefs(
      [ref(P, 'm2'), ref(P, 'm1'), ref(P, 'm2')],
      [ref(P, 'm1'), ref(P, 'm2')],
      [ref(P, 'm1'), ref(P, 'm2')],
    );

    expect(result.retained).toEqual([ref(P, 'm2'), ref(P, 'm1')]);
    expect(result.active).toEqual([ref(P, 'm2'), ref(P, 'm1')]);
    expect(result.removed).toEqual([]);
  });

  test('compares model reference arrays with order sensitivity', () => {
    expect(sameModelRefs([ref(P, 'm1'), ref(P, 'm2')], [ref(P, 'm1'), ref(P, 'm2')])).toBe(true);
    expect(sameModelRefs([ref(P, 'm1'), ref(P, 'm2')], [ref(P, 'm2'), ref(P, 'm1')])).toBe(false);
    expect(sameModelRefs([ref(P, 'm1')], [ref(P, 'm1'), ref(P, 'm2')])).toBe(false);
  });
});
