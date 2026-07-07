/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const source = readFileSync(new URL('./PinnedPlan.tsx', import.meta.url), 'utf8');
const nomiChatSource = readFileSync(new URL('../../platforms/nomi/NomiChat.tsx', import.meta.url), 'utf8');
const nomiSendBoxSource = readFileSync(new URL('../../platforms/nomi/NomiSendBox.tsx', import.meta.url), 'utf8');
const sendBoxSource = readFileSync(new URL('../../../../components/chat/SendBox/index.tsx', import.meta.url), 'utf8');

describe('PinnedPlan compact composer layout', () => {
  test('renders as a compact internal status surface instead of a floating full-width bar', () => {
    expect(source.includes("data-testid='pinned-plan-bar'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-summary'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-progress'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-list'")).toBe(true);
    expect(source.includes('sm:w-[56%]')).toBe(false);
    expect(source.includes('max-w-[520px]')).toBe(false);
    expect(source.includes('max-w-[420px]')).toBe(true);
    expect(source.includes('h-28px')).toBe(true);
    expect(source.includes('rd-999px')).toBe(true);
    expect(source.includes('min-w-0')).toBe(true);
    expect(source.includes("background: 'var(--color-bg-2)'")).toBe(false);
    expect(source.includes("background: 'var(--color-fill-1)'")).toBe(false);
    expect(source.includes("boxShadow: 'none'")).toBe(false);
    expect(source.includes('h-3px w-full')).toBe(false);
    expect(sendBoxSource.includes('bottom-[calc(100%+4px)]')).toBe(false);
    expect(source.includes('w-full max-w-800px')).toBe(false);
  });

  test('is docked inside the sendbox panel so it cannot cover the command queue', () => {
    expect(nomiChatSource.includes('<PinnedPlan />')).toBe(false);
    expect(nomiSendBoxSource.includes('showPinnedPlan')).toBe(true);
    expect(sendBoxSource.includes('showPinnedPlan?: boolean')).toBe(true);
    expect(sendBoxSource.includes('topRightTools?: React.ReactNode')).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-plan-overlay'")).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-top-right-tools'")).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-status-row'")).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-plan'")).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-context-tools'")).toBe(true);
    expect(sendBoxSource.includes('max-w-[420px]')).toBe(true);
    expect(sendBoxSource.includes('flex-[1_1_340px]')).toBe(true);
    expect(sendBoxSource.includes('absolute left-0 right-0 bottom-[calc(100%+4px)]')).toBe(false);
    expect(sendBoxSource.includes('absolute right-4px bottom-[calc(100%+4px)] h-36px')).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-top-row'")).toBe(false);
    expect(sendBoxSource.includes('top-1/2 -translate-y-1/2')).toBe(false);
    expect(nomiSendBoxSource.includes("data-testid='nomi-context-usage-slot'")).toBe(false);
    expect(nomiSendBoxSource.includes('topRightTools=')).toBe(false);

    const panelIndex = sendBoxSource.indexOf('sendbox-panel relative');
    const pinnedIndex = sendBoxSource.indexOf("data-testid='sendbox-internal-status-row'");
    const prefixIndex = sendBoxSource.indexOf('{prefix}', panelIndex);
    expect(pinnedIndex).toBeGreaterThan(-1);
    expect(panelIndex).toBeGreaterThan(-1);
    expect(pinnedIndex).toBeGreaterThan(panelIndex);
    expect(prefixIndex).toBeGreaterThan(pinnedIndex);
  });
});
