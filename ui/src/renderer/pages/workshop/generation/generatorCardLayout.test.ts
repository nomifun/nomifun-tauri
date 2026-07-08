/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL): string => readFileSync(url, 'utf8');

describe('workshop generator card responsive layout', () => {
  test('does not mask horizontal overflow in the card body', () => {
    const source = readSource(new URL('./GeneratorCard.tsx', import.meta.url));

    expect(source.includes('overflow-x-hidden')).toBe(false);
  });

  test('uses non-scrollable block segmented tabs inside fixed-width cards', () => {
    const source = readSource(new URL('../../../components/base/SegmentedTabs.tsx', import.meta.url));

    expect(source.includes("block ? 'flex w-full box-border overflow-visible' : 'inline-flex max-w-full box-border overflow-x-auto scrollbar-hide'")).toBe(true);
  });

  test('lets parameter rows fill and wrap instead of overflowing sideways', () => {
    const source = readSource(new URL('./ParamControls.tsx', import.meta.url));

    expect(source.includes("fill ? 'flex w-full flex-wrap gap-4px' : 'flex flex-wrap gap-4px'")).toBe(true);
    expect(source.includes("fill ? 'box-border flex-1 min-w-max max-w-full' : ''")).toBe(true);
    expect(source.includes('grid w-full grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]')).toBe(true);
  });

  test('uses border-box for full-width padded controls so the node border does not clip them', () => {
    const segmentedTabs = readSource(new URL('../../../components/base/SegmentedTabs.tsx', import.meta.url));
    const generatorCard = readSource(new URL('./GeneratorCard.tsx', import.meta.url));
    const modelPicker = readSource(new URL('./ModelPicker.tsx', import.meta.url));
    const promptField = readSource(new URL('./PromptField.tsx', import.meta.url));
    const paramControls = readSource(new URL('./ParamControls.tsx', import.meta.url));

    expect(segmentedTabs.includes("block ? 'flex w-full box-border overflow-visible'")).toBe(true);
    expect(modelPicker.includes('nodrag flex w-full box-border items-center')).toBe(true);
    expect(promptField.includes('nodrag nowheel w-full box-border resize-none')).toBe(true);
    expect(paramControls.includes('nodrag w-full min-w-0 box-border rounded-7px')).toBe(true);
    expect(generatorCard.includes('nodrag flex w-full box-border items-center justify-center')).toBe(true);
  });
});
