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

describe('PinnedPlan composer popover layout', () => {
  test('uses the conversation surface with a content-fit queue capsule and fixed text spacing', () => {
    expect(source.includes("data-testid='pinned-plan-bar'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-summary'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-progress'")).toBe(false);
    expect(source.includes("data-testid='pinned-plan-progress-indicator'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-list'")).toBe(true);
    expect(source.includes('sm:w-[56%]')).toBe(false);
    expect(source.includes('max-w-[520px]')).toBe(false);
    expect(source.includes('w-fit max-w-[calc(100vw-32px)]')).toBe(true);
    expect(source.includes('h-28px')).toBe(true);
    expect(source.includes('rd-999px')).toBe(true);
    expect(source.includes('min-w-0')).toBe(true);
    expect(source.includes("background: 'var(--color-bg-1)'")).toBe(true);
    expect(source.includes("boxShadow: 'none'")).toBe(true);
    expect(source.includes('h-3px w-full')).toBe(false);
    expect(source.includes('animate-spin')).toBe(true);
    expect(source.includes('{done < total && (')).toBe(true);
    expect(source.includes("className='ml-18px whitespace-nowrap text-12px leading-none tabular-nums'")).toBe(true);
    expect(source.includes("data-testid='pinned-plan-popover'")).toBe(true);
    expect(source.includes('absolute left-1/2 w-[min(320px,calc(100vw-32px))] -translate-x-1/2')).toBe(true);
    expect(source.includes('min-w-0 flex-1 line-clamp-2')).toBe(true);
    expect(source.includes('onMouseEnter={handleDesktopOpen}')).toBe(true);
    expect(source.includes('onMouseLeave={handleDesktopClose}')).toBe(true);
    expect(source.includes('if (!isMobile) return;')).toBe(true);
    expect(source.includes('w-full max-w-800px')).toBe(false);
  });

  test('is centered above the sendbox panel rather than inside its status row', () => {
    expect(nomiChatSource.includes('<PinnedPlan />')).toBe(false);
    expect(nomiSendBoxSource.includes('showPinnedPlan')).toBe(true);
    expect(sendBoxSource.includes('showPinnedPlan?: boolean')).toBe(true);
    expect(sendBoxSource.includes('topRightTools?: React.ReactNode')).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-plan-anchor'")).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-top-right-tools'")).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-status-row'")).toBe(true);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-plan'")).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-internal-context-tools'")).toBe(true);
    expect(sendBoxSource.includes('absolute left-1/2 bottom-[calc(100%+8px)] -translate-x-1/2')).toBe(true);
    expect(sendBoxSource.includes('max-w-[420px]')).toBe(false);
    expect(sendBoxSource.includes('flex-[1_1_340px]')).toBe(false);
    expect(sendBoxSource.includes("data-testid='sendbox-top-row'")).toBe(false);
    expect(sendBoxSource.includes('top-1/2 -translate-y-1/2')).toBe(false);
    expect(nomiSendBoxSource.includes("data-testid='nomi-context-usage-slot'")).toBe(false);
    expect(nomiSendBoxSource.includes('topRightTools=')).toBe(false);

    const panelIndex = sendBoxSource.indexOf('sendbox-panel relative');
    const anchorIndex = sendBoxSource.indexOf("data-testid='sendbox-plan-anchor'");
    const pinnedIndex = sendBoxSource.indexOf("data-testid='sendbox-internal-status-row'");
    expect(anchorIndex).toBeGreaterThan(-1);
    expect(panelIndex).toBeGreaterThan(-1);
    expect(anchorIndex).toBeLessThan(panelIndex);
    expect(pinnedIndex).toBeGreaterThan(panelIndex);
  });
});
