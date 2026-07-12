import { describe, expect, test } from 'bun:test';

import type { TModelRef } from '@/common/types/orchestrator/orchestratorTypes';
import { reconcileModelRefs, sameModelRefs } from './collaboratorModelRefs';

const ref = (provider_id: string, model: string): TModelRef => ({ provider_id, model });

describe('collaborator model reference reconciliation', () => {
  test('removes a deleted provider while preserving valid order', () => {
    const result = reconcileModelRefs(
      [ref('gone', 'g1'), ref('keep', 'k2'), ref('keep', 'k1')],
      [ref('keep', 'k1'), ref('keep', 'k2')],
      [ref('keep', 'k1'), ref('keep', 'k2')]
    );

    expect(result.retained).toEqual([ref('keep', 'k2'), ref('keep', 'k1')]);
    expect(result.active).toEqual(result.retained);
    expect(result.removed).toEqual([ref('gone', 'g1')]);
  });

  test('removes a deleted model from a surviving provider', () => {
    const result = reconcileModelRefs(
      [ref('keep', 'removed-model'), ref('keep', 'live-model')],
      [ref('keep', 'live-model')],
      [ref('keep', 'live-model')]
    );

    expect(result.retained).toEqual([ref('keep', 'live-model')]);
    expect(result.active).toEqual([ref('keep', 'live-model')]);
    expect(result.removed).toEqual([ref('keep', 'removed-model')]);
  });

  test('retains but deactivates disabled models', () => {
    const result = reconcileModelRefs(
      [ref('p', 'disabled'), ref('p', 'enabled')],
      [ref('p', 'disabled'), ref('p', 'enabled')],
      [ref('p', 'enabled')]
    );

    expect(result.retained).toEqual([ref('p', 'disabled'), ref('p', 'enabled')]);
    expect(result.active).toEqual([ref('p', 'enabled')]);
    expect(result.removed).toEqual([]);
  });

  test('deduplicates references while preserving the first occurrence', () => {
    const result = reconcileModelRefs(
      [ref('p', 'm2'), ref('p', 'm1'), ref('p', 'm2')],
      [ref('p', 'm1'), ref('p', 'm2')],
      [ref('p', 'm1'), ref('p', 'm2')]
    );

    expect(result.retained).toEqual([ref('p', 'm2'), ref('p', 'm1')]);
    expect(result.active).toEqual([ref('p', 'm2'), ref('p', 'm1')]);
    expect(result.removed).toEqual([]);
  });

  test('compares model reference arrays with order sensitivity', () => {
    expect(sameModelRefs([ref('p', 'm1'), ref('p', 'm2')], [ref('p', 'm1'), ref('p', 'm2')])).toBe(true);
    expect(sameModelRefs([ref('p', 'm1'), ref('p', 'm2')], [ref('p', 'm2'), ref('p', 'm1')])).toBe(false);
    expect(sameModelRefs([ref('p', 'm1')], [ref('p', 'm1'), ref('p', 'm2')])).toBe(false);
  });
});
