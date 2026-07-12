/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const stylesheet = readFileSync(new URL('./chat-layout.css', import.meta.url), 'utf8');
const componentSource = readFileSync(new URL('./WorkspaceToolRail.tsx', import.meta.url), 'utf8');

const rule = (selector: string) => {
  const match = stylesheet.match(new RegExp(`${selector}\\s*\\{([\\s\\S]*?)\\n\\}`, 'm'));
  expect(match).not.toBeNull();
  return match?.[1] ?? '';
};

describe('workspace tool rail dimensions', () => {
  test('uses compact square desktop controls', () => {
    const rail = rule('\\.workspace-tool-rail');
    const item = rule('\\.workspace-tool-rail__item');
    const collapse = rule('\\.workspace-tool-rail__item--collapse');

    expect(rail.includes('flex: 0 0 32px;')).toBe(true);
    expect(rail.includes('width: 32px;')).toBe(true);
    expect(rail.includes('min-width: 32px;')).toBe(true);
    expect(item.includes('width: 28px;')).toBe(true);
    expect(item.includes('height: 28px;')).toBe(true);
    expect(item.includes('aspect-ratio: 1 / 1;')).toBe(true);
    expect(collapse.includes('height: 28px;')).toBe(true);
  });

  test('does not change the mobile workspace trigger dimensions', () => {
    const trigger = rule('\\.workspace-tool-rail-mobile-trigger');

    expect(trigger.includes('width: 24px;')).toBe(true);
    expect(trigger.includes('height: 70px;')).toBe(true);
  });

  test('keeps labels accessible but visually hidden beneath icon-only controls', () => {
    const label = rule('\\.workspace-tool-rail__label');

    expect(componentSource.includes("className='workspace-tool-rail__label'")).toBe(true);
    expect(label.includes('position: absolute;')).toBe(true);
    expect(label.includes('width: 1px;')).toBe(true);
    expect(label.includes('height: 1px;')).toBe(true);
    expect(label.includes('overflow: hidden;')).toBe(true);
  });

  test('uses a compact scoped tooltip and removes the active vertical bar', () => {
    const tooltip = rule('\\.workspace-tool-rail__tooltip \\.arco-tooltip-content');

    expect(componentSource.includes("mini className='workspace-tool-rail__tooltip'")).toBe(true);
    expect(stylesheet.includes('.workspace-tool-rail__item--active::before')).toBe(false);
    expect(tooltip.includes('font-size: 11px;')).toBe(true);
    expect(tooltip.includes('line-height: 16px;')).toBe(true);
    expect(tooltip.includes('padding: 3px 6px;')).toBe(true);
  });
});
