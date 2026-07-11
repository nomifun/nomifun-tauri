import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { FilterTrigger } from './RequirementFilters';

const filtersSource = readFileSync(new URL('./RequirementFilters.tsx', import.meta.url), 'utf8');
const listViewSource = readFileSync(new URL('./RequirementListView.tsx', import.meta.url), 'utf8');
const workspaceSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');

describe('RequirementFilters trigger', () => {
  test('forwards a DOM ref so Arco can anchor the popup', () => {
    expect((FilterTrigger as unknown as { $$typeof?: symbol }).$$typeof).toBe(Symbol.for('react.forward_ref'));
  });

  test('renders icon, function label, and selected content', () => {
    const html = renderToStaticMarkup(<FilterTrigger icon={<span>icon</span>} label='标签' value='产品' />);

    expect(html.includes('icon')).toBe(true);
    expect(html.includes('标签')).toBe(true);
    expect(html.includes('产品')).toBe(true);
    expect(html.includes('aria-label="标签: 产品"')).toBe(true);
  });

  test('omits selected content when the filter is inactive', () => {
    const html = renderToStaticMarkup(<FilterTrigger icon={<span>icon</span>} label='状态' />);

    expect(html.includes('aria-label="状态"')).toBe(true);
    expect(html.includes('undefined')).toBe(false);
  });

  test('uses the primary active color when selected or open', () => {
    const html = renderToStaticMarkup(
      <FilterTrigger icon={<span>icon</span>} label='标签' value='产品' active />
    );

    expect(html.includes('aria-pressed="true"')).toBe(true);
    expect(html.includes('!bg-primary-1')).toBe(true);
    expect(html.includes('!text-primary-6')).toBe(true);
  });

  test('gives the selected value a smaller neutral text hierarchy', () => {
    const html = renderToStaticMarkup(
      <FilterTrigger icon={<span>icon</span>} label='标签' value='五子棋' active />
    );

    expect(html.includes('text-12px')).toBe(true);
    expect(html.includes('font-medium text-[var(--color-text-1)]')).toBe(true);
    expect(html.includes('ml-2px')).toBe(true);
  });

  test('keeps select-all controls in the filter row instead of a separate list header', () => {
    expect(filtersSource.includes('requirements.selection.selectAllPage')).toBe(true);
    expect(listViewSource.includes('requirements.selection.selectAllPage')).toBe(false);
  });

  test('separates the filter row from content without adding an outer border', () => {
    expect(workspaceSource.includes("<div className='mt-8px'>")).toBe(true);
    expect(workspaceSource.includes("<div className='mt-10px'>")).toBe(true);
    expect(
      filtersSource.includes(
        "role='separator' className='mt-6px h-px bg-[var(--color-border-2)]'"
      )
    ).toBe(true);
    expect(
      filtersSource.includes(
        "border-b border-solid border-[var(--color-border-2)] pb-6px"
      )
    ).toBe(false);
  });
});
