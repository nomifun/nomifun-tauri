/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import {
  filterKnowledgeBasesByQuery,
  shouldShowKnowledgeBaseSearch,
} from './KnowledgeControl.utils';

const bases = [
  { id: 'finance-live', name: '财联社新闻-实时', kind: 'web', tags: ['news'], file_count: 0 },
  { id: 'product-docs', name: '产品知识库', kind: 'local', tags: ['docs'], file_count: 12 },
  { id: 'blank-notes', name: '回写暂存', kind: 'blank', tags: [], file_count: 0 },
];

describe('KnowledgeControl search helpers', () => {
  test('shows search as soon as there is more than one knowledge base to choose from', () => {
    expect(shouldShowKnowledgeBaseSearch(0)).toBe(false);
    expect(shouldShowKnowledgeBaseSearch(1)).toBe(false);
    expect(shouldShowKnowledgeBaseSearch(2)).toBe(true);
  });

  test('filters by knowledge base name, tag label, and kind label', () => {
    const tagLabels = { news: '财经新闻', docs: '项目文档' };
    const kindLabels = { web: '网页', local: '本地文件夹', blank: '空白' };

    expect(filterKnowledgeBasesByQuery(bases, ' 实时 ', tagLabels, kindLabels).map((b) => b.id)).toEqual([
      'finance-live',
    ]);
    expect(filterKnowledgeBasesByQuery(bases, '文档', tagLabels, kindLabels).map((b) => b.id)).toEqual([
      'product-docs',
    ]);
    expect(filterKnowledgeBasesByQuery(bases, '网页', tagLabels, kindLabels).map((b) => b.id)).toEqual([
      'finance-live',
    ]);
    expect(filterKnowledgeBasesByQuery(bases, '不存在', tagLabels, kindLabels)).toEqual([]);
    expect(filterKnowledgeBasesByQuery(bases, '', tagLabels, kindLabels)).toEqual(bases);
  });

  test('uses theme-aware selected states without white text or white hit targets', () => {
    const source = readFileSync(new URL('./KnowledgeControl.tsx', import.meta.url), 'utf8');

    expect(source.includes('text-white')).toBe(false);
    expect(source.includes('bg-[rgb(var(--primary-6))]')).toBe(false);
    expect(source.includes('border-[rgba(var(--primary-6),0.38)]')).toBe(true);
    expect(source.includes('bg-[rgba(var(--primary-6),0.12)] text-[rgb(var(--primary-6))]')).toBe(true);
  });

  test('keeps kind and freshness/file-count metadata on the title row', () => {
    const source = readFileSync(new URL('./KnowledgeControl.tsx', import.meta.url), 'utf8');

    expect(source.includes('knowledge-control-base-meta')).toBe(true);
    expect(source.includes('mt-2px flex items-center gap-8px text-11px text-[var(--color-text-2)]')).toBe(false);
  });

  test('keeps unavailable local-folder bases visible but non-selectable', () => {
    const source = readFileSync(new URL('./KnowledgeControl.tsx', import.meta.url), 'utf8');

    expect(source.includes('const rootMissing = !base.root_exists;')).toBe(true);
    expect(source.includes('knowledge-control-root-missing')).toBe(true);
    expect(
      source.includes(
        '!targetUnresolved && (!rootMissing || isSelected) && handleToggleBase(base.knowledge_base_id)'
      )
    ).toBe(true);
    expect(source.includes("t('knowledge.mount.rootMissing'")).toBe(true);
  });

  test('refreshes the mounted binding when another surface changes the same target', () => {
    const source = readFileSync(new URL('./KnowledgeControl.tsx', import.meta.url), 'utf8');

    expect(source.includes('ipcBridge.knowledge.onBindingChanged.on')).toBe(true);
    expect(source.includes('reloadBinding')).toBe(true);
    expect(source.includes('event.target_kind')).toBe(true);
    expect(source.includes('event.target_id')).toBe(true);
  });
});
