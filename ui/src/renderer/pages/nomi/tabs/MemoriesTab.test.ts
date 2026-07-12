/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./MemoriesTab.tsx', import.meta.url), 'utf8');

describe('desktop companion memories pagination', () => {
  test('requests 10 memories by default and renders a controllable pager', () => {
    expect(source.includes('const [page, setPage] = useState(1);')).toBe(true);
    expect(source.includes('const [pageSize, setPageSize] = useState(10);')).toBe(true);
    expect(source.includes('limit: pageSize')).toBe(true);
    expect(source.includes('offset: (page - 1) * pageSize')).toBe(true);
    expect(source.includes('<Pagination')).toBe(true);
    expect(source.includes('sizeCanChange')).toBe(true);
    expect(source.includes('sizeOptions={[10, 20, 50]}')).toBe(true);
  });

  test('keeps memory content prominent and groups secondary actions in a menu', () => {
    expect(source.includes('line-clamp-2')).toBe(true);
    expect(source.includes('<Dropdown')).toBe(true);
    expect(source.includes('<More')).toBe(true);
  });
});
