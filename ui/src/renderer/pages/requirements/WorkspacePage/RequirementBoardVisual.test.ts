/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const boardSource = readFileSync(new URL('./RequirementBoardView.tsx', import.meta.url), 'utf8');
const cardSource = readFileSync(new URL('./RequirementBoardCard.tsx', import.meta.url), 'utf8');
const controlCss = readFileSync(new URL('../../../styles/theme-control-contract.css', import.meta.url), 'utf8');
const workspaceSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
const layoutSource = readFileSync(new URL('../RequirementsLayout/index.tsx', import.meta.url), 'utf8');

describe('requirements board visual hierarchy', () => {
  test('uses dedicated surfaces for columns and cards in both light and dark themes', () => {
    expect(boardSource.includes('requirements-board-column')).toBe(true);
    expect(boardSource.includes('flex w-full flex-1 min-h-0 items-start gap-12px overflow-x-auto pb-4px')).toBe(true);
    expect(boardSource.includes('h-[calc(100vh-312px)]')).toBe(false);
    expect(boardSource.includes("const hasItems = colItems.length > 0;")).toBe(true);
    expect(boardSource.includes("hasItems ? 'self-stretch' : ''")).toBe(true);
    expect(boardSource.includes("hasItems ? 'flex-1 min-h-0' : ''")).toBe(true);
    expect(cardSource.includes('requirements-board-card')).toBe(true);
    expect(cardSource.includes('rounded-10px')).toBe(true);
    expect(controlCss.includes('.requirements-board-column')).toBe(true);
    expect(controlCss.includes('background-color: var(--color-bg-white) !important;')).toBe(true);
    expect(controlCss.includes("[data-theme='dark'] .requirements-board-card")).toBe(true);
    expect(controlCss.includes('background-color: var(--color-bg-3) !important;')).toBe(true);
    expect(controlCss.includes('margin-right: -8px;')).toBe(true);
    expect(controlCss.includes('scrollbar-gutter: stable;')).toBe(true);
    expect(workspaceSource.includes("className='flex h-full min-h-0 flex-col'")).toBe(true);
    expect(workspaceSource.includes("view === 'board' ? 'mt-10px flex flex-1 min-h-0' : 'mt-10px'")).toBe(true);
    expect(layoutSource.includes('box-border h-full pt-32px pb-4px')).toBe(true);
  });
});
