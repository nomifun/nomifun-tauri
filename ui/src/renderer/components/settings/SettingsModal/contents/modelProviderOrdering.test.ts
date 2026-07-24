import { describe, expect, test } from 'bun:test';

import type { IProvider } from '@/common/config/storage';
import { parseProviderId, type ProviderId } from '@/common/types/ids';
import { reorderById, reorderStrings, withDenseSortOrder } from './modelProviderOrdering';

const A = parseProviderId('0190f5fe-7c00-7a00-8000-000000000001');
const B = parseProviderId('0190f5fe-7c00-7a00-8000-000000000002');
const C = parseProviderId('0190f5fe-7c00-7a00-8000-000000000003');
const MISSING = parseProviderId('0190f5fe-7c00-7a00-8000-000000000004');

const provider = (id: ProviderId, sort_order?: number): IProvider =>
  ({
    id,
    platform: 'openai',
    name: id,
    base_url: 'https://example.com',
    api_key: 'sk-test',
    models: [],
    sort_order,
  });

describe('modelProviderOrdering', () => {
  test('reorderById moves provider rows by id', () => {
    const result = reorderById([provider(A), provider(B), provider(C)], C, A);
    expect(result.map((item) => item.id)).toEqual([C, A, B]);
  });

  test('reorderById returns the original array for invalid or same targets', () => {
    const input = [provider(A), provider(B)];
    expect(reorderById(input, MISSING, A)).toBe(input);
    expect(reorderById(input, A, MISSING)).toBe(input);
    expect(reorderById(input, A, A)).toBe(input);
  });

  test('reorderStrings moves model ids', () => {
    expect(reorderStrings(['m1', 'm2', 'm3'], 'm1', 'm3')).toEqual(['m2', 'm3', 'm1']);
  });

  test('withDenseSortOrder rewrites provider priority by visual position', () => {
    const result = withDenseSortOrder([provider(B, 10), provider(A, 3)]);
    expect(result.map((item) => [item.id, item.sort_order])).toEqual([
      [B, 0],
      [A, 1],
    ]);
  });
});
