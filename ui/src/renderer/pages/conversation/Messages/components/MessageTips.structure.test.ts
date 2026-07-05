/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./MessageTips.tsx', import.meta.url), 'utf8');
const cssSource = readFileSync(new URL('../messages.css', import.meta.url), 'utf8');

const cssRuleFor = (selector: string) => {
  const escapedSelector = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = cssSource.match(new RegExp(`${escapedSelector}\\s*\\{([^}]*)\\}`));
  return match?.[1] ?? '';
};

describe('MessageTips structured error presentation', () => {
  test('uses a NomiFun-native diagnostic note instead of the legacy open-source alert block', () => {
    expect(source.includes('message-error-note')).toBe(true);
    expect(source.includes('message-error-note__rail')).toBe(true);
    expect(source.includes('message-error-note__status')).toBe(true);
    expect(source.includes('message-error-note__details')).toBe(true);
    expect(source.includes("defaultActiveKey={['technical-details']}")).toBe(false);
  });

  test('keeps feedback and retry/configuration status close to the diagnosis header', () => {
    expect(source.includes('message-error-note__meta')).toBe(true);
    expect(source.includes('message-error-note__actions')).toBe(true);
  });

  test('aligns readable content on one text axis instead of drifting under the icon', () => {
    expect(source.includes('message-error-note__main')).toBe(true);
    expect(cssSource.includes('.message-error-note__main')).toBe(true);
    expect(cssSource.includes('margin-left: 32px')).toBe(true);
  });

  test('uses a stable footer bar so details and feedback do not sit on different baselines', () => {
    expect(source.includes('message-error-note__footer-main')).toBe(true);
    expect(cssSource.includes('align-items: center')).toBe(true);
    expect(cssSource.includes('min-height: 32px')).toBe(true);
  });

  test('keeps the technical-details icon inline before the label instead of overlaying it', () => {
    const headerRule = cssRuleFor('.message-error-note__details .arco-collapse-item-header');
    const iconRule = cssRuleFor('.message-error-note__details .arco-collapse-item-icon-hover');

    expect(headerRule.includes('display: flex')).toBe(true);
    expect(headerRule.includes('align-items: center')).toBe(true);
    expect(headerRule.includes('justify-content: flex-start')).toBe(true);
    expect(iconRule.includes('position: static')).toBe(true);
    expect(iconRule.includes('width: 16px')).toBe(true);
    expect(iconRule.includes('transform: none')).toBe(true);
  });

  test('pins the feedback action to a centered footer slot without inherited icon offset', () => {
    expect(cssSource.includes('grid-template-columns: minmax(0, 1fr) auto')).toBe(true);
    expect(cssSource.includes('height: 28px')).toBe(true);
    expect(cssSource.includes('.message-error-note__actions .message-error-note__feedback')).toBe(true);
    expect(cssSource.includes('.message-error-note__feedback .pt-4px')).toBe(true);
    expect(cssSource.includes('padding-top: 0 !important')).toBe(true);
  });
});
