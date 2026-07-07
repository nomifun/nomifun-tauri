/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const cssSource = readFileSync(new URL('./messages.css', import.meta.url), 'utf8');
const disclosureSource = readFileSync(new URL('./components/TurnProcessDisclosure.tsx', import.meta.url), 'utf8');
const messageListSource = readFileSync(new URL('./MessageList.tsx', import.meta.url), 'utf8');

const cssRuleFor = (selector: string) => {
  const escapedSelector = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = cssSource.match(new RegExp(`${escapedSelector}\\s*\\{([^}]*)\\}`));
  return match?.[1] ?? '';
};

describe('turn process disclosure content layout', () => {
  test('tags disclosure items by content kind for grouped spacing', () => {
    expect(disclosureSource.includes('getProcessItemLayoutKind')).toBe(true);
    expect(disclosureSource.includes('turn-process-disclosure__item--${layoutKind}')).toBe(true);
    expect(messageListSource.includes('getProcessItemLayoutKind={getProcessItemLayoutKind}')).toBe(true);
  });

  test('keeps the disclosure timer live while the current turn is running', () => {
    expect(disclosureSource.includes('running: boolean')).toBe(true);
    expect(disclosureSource.includes('if (!item.running) return;')).toBe(true);
    expect(disclosureSource.includes('window.setInterval')).toBe(true);
    expect(disclosureSource.includes('const durationEndAt = item.running ? now : item.endAt;')).toBe(true);
    expect(messageListSource.includes('running: entry.running')).toBe(true);
  });

  test('does not render an empty disclosure body before process rows arrive', () => {
    expect(disclosureSource.includes('const hasProcessItems = item.processItems.length > 0')).toBe(true);
    expect(disclosureSource.includes('const disclosureExpanded = hasProcessItems && expanded')).toBe(true);
    expect(disclosureSource.includes('{hasProcessItems && (')).toBe(true);
    expect(disclosureSource.includes('{disclosureExpanded && (')).toBe(true);
  });

  test('offers a one-click control to expand all completed thinking blocks', () => {
    expect(disclosureSource.includes('getProcessItemCanExpandAll')).toBe(true);
    expect(disclosureSource.includes('expandAllProcessItemKeys')).toBe(true);
    expect(disclosureSource.includes('turn-process-disclosure__expand-thinking')).toBe(true);
    expect(disclosureSource.includes('const hasExpandableProcessItems = expandableProcessItemKeys.length > 0')).toBe(true);
    expect(disclosureSource.includes('const allExpandableProcessItemsExpanded =')).toBe(true);
    expect(disclosureSource.includes('setExpandAllProcessItemKeys(new Set())')).toBe(true);
    expect(disclosureSource.includes('turn-process-disclosure__header-actions')).toBe(true);
    expect(disclosureSource.includes('turn-process-disclosure__toggle')).toBe(true);
    expect(disclosureSource.includes("messages.turnProcess.expandAllThinkingProcess")).toBe(true);
    expect(disclosureSource.includes("messages.turnProcess.collapseAllThinkingProcess")).toBe(true);
    expect(disclosureSource.indexOf("className='turn-process-disclosure__header-actions'")).toBeGreaterThan(
      disclosureSource.indexOf('turn-process-disclosure__header')
    );
    expect(disclosureSource.indexOf("className='turn-process-disclosure__header-actions'")).toBeLessThan(
      disclosureSource.indexOf("className='turn-process-disclosure__body'")
    );
    expect(cssSource.includes('.turn-process-disclosure__header-actions')).toBe(true);
    expect(cssSource.includes('.turn-process-disclosure__toggle')).toBe(true);
    expect(messageListSource.includes('getProcessItemCanExpandAll={isCompletedThinkingProcessItem}')).toBe(true);
    expect(disclosureSource.includes('expanded: expandAllProcessItemKeys.has(itemKey)')).toBe(true);
    expect(messageListSource.includes('expansionControls')).toBe(true);
  });

  test('uses tighter same-kind spacing and clearer cross-kind spacing', () => {
    expect(cssRuleFor('.turn-process-disclosure__body').includes('gap: 0')).toBe(true);
    expect(cssRuleFor('.turn-process-disclosure__item').includes('margin-top: 14px')).toBe(true);
    expect(cssSource.includes('.turn-process-disclosure__item:first-child')).toBe(true);
    expect(cssSource.includes('.turn-process-disclosure__item--text + .turn-process-disclosure__item--text')).toBe(true);
    expect(cssSource.includes('.turn-process-disclosure__item--tool + .turn-process-disclosure__item--tool')).toBe(true);
  });

  test('separates paragraph text from lightweight process rows', () => {
    const paragraphRule = cssRuleFor('.turn-process-disclosure__body .turn-process-trace__paragraph');
    const rowRule = cssRuleFor('.turn-process-disclosure__body .turn-process-trace__row');

    expect(paragraphRule.includes('color: var(--color-text-1')).toBe(true);
    expect(paragraphRule.includes('font-size: var(--conversation-message-font-size)')).toBe(true);
    expect(paragraphRule.includes('line-height: var(--conversation-message-line-height)')).toBe(true);
    expect(rowRule.includes('color: var(--color-text-3')).toBe(true);
    expect(rowRule.includes('font-size: var(--conversation-message-font-size)')).toBe(true);
    expect(rowRule.includes('line-height: var(--conversation-message-line-height)')).toBe(true);
  });

  test('keeps running process chrome neutral instead of bright blue', () => {
    expect(cssRuleFor('.turn-process-disclosure--running').includes('color: var(--color-text-3')).toBe(true);
    expect(
      cssRuleFor('.turn-process-disclosure__body .turn-process-trace__row--running').includes(
        'color: var(--color-text-3'
      )
    ).toBe(true);
    expect(cssRuleFor('.turn-process-trace__row--running').includes('color: var(--color-text-3')).toBe(true);
  });

  test('defines the shared conversation body typography token once', () => {
    const itemRule = cssRuleFor('.message-item');

    expect(itemRule.includes('--conversation-message-font-size: 14px')).toBe(true);
    expect(itemRule.includes('--conversation-message-line-height: 22px')).toBe(true);
    expect(cssSource.includes('.message-item .markdown-shadow p')).toBe(false);
    expect(cssSource.includes('.message-item .whitespace-pre-wrap')).toBe(false);
  });
});
