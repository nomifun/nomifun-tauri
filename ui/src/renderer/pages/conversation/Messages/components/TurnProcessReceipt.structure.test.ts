/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import TurnProcessReceipt from './TurnProcessReceipt';
import type { TurnProcessReceiptView } from './TurnProcessReceipt';

const source = readFileSync(new URL('./TurnProcessReceipt.tsx', import.meta.url), 'utf8');

type TestReceipt = TurnProcessReceiptView<{ id: string }>;

const baseReceipt: TestReceipt = {
  id: 'receipt-model-activity',
  item: { id: 'model-activity' },
  label: '正在准备下一步操作',
  state: 'running',
  icon: 'status',
  defaultExpanded: true,
};

describe('TurnProcessReceipt expandable detail structure', () => {
  test('supports non-expandable receipts when no detail content exists', () => {
    expect(source.includes('hasDetail')).toBe(true);
    expect(source.includes('receipt.hasDetail === true')).toBe(true);
    expect(source.includes('turn-process-receipt__header--static')).toBe(true);
    expect(source.includes('canExpand && expanded')).toBe(true);
  });

  test('treats omitted hasDetail as static and does not render duplicate detail', () => {
    const html = renderToStaticMarkup(
      React.createElement(TurnProcessReceipt, {
        receipt: baseReceipt,
        renderProcessItem: () => React.createElement('div', null, 'duplicated detail'),
      })
    );

    expect(html.includes('turn-process-receipt__header--static')).toBe(true);
    expect(html.includes('turn-process-receipt__arrow')).toBe(false);
    expect(html.includes('duplicated detail')).toBe(false);
  });

  test('renders a stable visible icon marker before every receipt label', () => {
    const iconCases: Array<[TestReceipt['icon'], string]> = [
      ['tool', 'terminal'],
      ['file', 'file'],
      ['edit', 'edit'],
      ['status', 'status'],
    ];

    for (const [icon, marker] of iconCases) {
      const html = renderToStaticMarkup(
        React.createElement(TurnProcessReceipt, {
          receipt: { ...baseReceipt, id: `receipt-${icon}`, icon, state: 'completed' },
          renderProcessItem: () => React.createElement('div', null, 'detail'),
        })
      );

      expect(html.includes(`data-receipt-icon="${marker}"`)).toBe(true);
      expect(html.indexOf('turn-process-receipt__icon')).toBeLessThan(html.indexOf('turn-process-receipt__label'));
    }
  });
});
