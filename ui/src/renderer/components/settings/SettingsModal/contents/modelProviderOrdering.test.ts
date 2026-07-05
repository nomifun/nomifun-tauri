import { describe, expect, test } from 'bun:test';

import type { IProvider } from '@/common/config/storage';
import { reorderById, reorderStrings, withDenseSortOrder } from './modelProviderOrdering';

const provider = (id: string, sort_order?: number): IProvider =>
  ({
    id,
    platform: 'openai',
    name: id,
    base_url: 'https://example.com',
    api_key: 'sk-test',
    models: [],
    sort_order,
  }) as IProvider;

describe('modelProviderOrdering', () => {
  test('reorderById moves provider rows by id', () => {
    const result = reorderById([provider('a'), provider('b'), provider('c')], 'c', 'a');
    expect(result.map((item) => item.id)).toEqual(['c', 'a', 'b']);
  });

  test('reorderById returns the original array for invalid or same targets', () => {
    const input = [provider('a'), provider('b')];
    expect(reorderById(input, 'missing', 'a')).toBe(input);
    expect(reorderById(input, 'a', 'missing')).toBe(input);
    expect(reorderById(input, 'a', 'a')).toBe(input);
  });

  test('reorderStrings moves model ids', () => {
    expect(reorderStrings(['m1', 'm2', 'm3'], 'm1', 'm3')).toEqual(['m2', 'm3', 'm1']);
  });

  test('withDenseSortOrder rewrites provider priority by visual position', () => {
    const result = withDenseSortOrder([provider('b', 10), provider('a', 3)]);
    expect(result.map((item) => [item.id, item.sort_order])).toEqual([
      ['b', 0],
      ['a', 1],
    ]);
  });
});
