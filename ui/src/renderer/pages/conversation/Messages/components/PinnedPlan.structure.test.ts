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
  test('renders as a centered short bar instead of filling the composer width', () => {
    expect(source.includes("data-testid='pinned-plan-bar'")).toBe(true);
    expect(source.includes('sm:w-[56%]')).toBe(true);
    expect(source.includes('max-w-[520px]')).toBe(true);
    expect(source.includes('min-w-0')).toBe(true);
    expect(source.includes("background: 'var(--color-bg-2)'")).toBe(true);
    expect(source.includes("background: 'var(--color-fill-1)'")).toBe(false);
    expect(sendBoxSource.includes('bottom-[calc(100%+4px)]')).toBe(true);
    expect(source.includes('w-full max-w-800px')).toBe(false);
  });

  test('is docked in the sendbox top row instead of floating above it', () => {
    expect(nomiChatSource.includes('<PinnedPlan />')).toBe(false);
    expect(nomiSendBoxSource.includes('showPinnedPlan')).toBe(true);
    expect(sendBoxSource.includes('showPinnedPlan?: boolean')).toBe(true);
    expect(sendBoxSource.includes('topRightTools?: React.ReactNode')).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-plan-overlay'")).toBe(true);
    expect(sendBoxSource.includes('absolute left-0 right-0 bottom-[calc(100%+4px)]')).toBe(true);
    expect(sendBoxSource.includes('pointer-events-none')).toBe(true);
    expect(sendBoxSource.includes('pointer-events-auto')).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-top-right-tools'")).toBe(true);
    expect(sendBoxSource.includes('absolute right-4px bottom-[calc(100%+4px)] h-36px')).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-top-row'")).toBe(false);
    expect(sendBoxSource.includes('top-1/2 -translate-y-1/2')).toBe(false);
    expect(nomiSendBoxSource.includes("data-testid='nomi-context-usage-slot'")).toBe(false);
    expect(nomiSendBoxSource.includes('topRightTools=')).toBe(true);

    const pinnedIndex = sendBoxSource.indexOf("data-testid='sendbox-plan-overlay'");
    const panelIndex = sendBoxSource.indexOf('sendbox-panel relative');
    expect(pinnedIndex).toBeGreaterThan(-1);
    expect(panelIndex).toBeGreaterThan(pinnedIndex);
  });
});
