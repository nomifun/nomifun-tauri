/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const badgeSource = readFileSync(
  new URL('../components/RequirementDisplayNumber.tsx', import.meta.url),
  'utf8'
);
const rowSource = readFileSync(new URL('./RequirementListRow.tsx', import.meta.url), 'utf8');
const boardSource = readFileSync(new URL('./RequirementBoardCard.tsx', import.meta.url), 'utf8');
const drawerSource = readFileSync(new URL('../RequirementDrawer/index.tsx', import.meta.url), 'utf8');

describe('requirement human-facing display number', () => {
  test('renders a compact #N badge that copies the canonical ID accessibly', () => {
    expect(badgeSource.includes('`#${displayNo}`')).toBe(true);
    expect(badgeSource.includes('copyText(fullId)')).toBe(true);
    expect(badgeSource.includes("role={fullId ? 'button' : undefined}")).toBe(true);
    expect(badgeSource.includes("event.key === 'Enter' || event.key === ' '")).toBe(true);
    expect(badgeSource.includes('min-w-48px')).toBe(true);
  });

  test('uses display_no instead of exposing the canonical UUID in workspace surfaces', () => {
    expect(rowSource.includes('displayNo={item.display_no} fullId={item.requirement_id}')).toBe(true);
    expect(rowSource.includes('style={{ fontVariantNumeric')).toBe(false);
    expect(boardSource.includes('CopyIconButton text={item.requirement_id}')).toBe(true);
    expect(drawerSource.includes('displayNo={data.display_no}')).toBe(true);
    expect(drawerSource.includes('<CopyFullIdButton id={data.requirement_id} />')).toBe(true);
  });
});
