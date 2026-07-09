/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { readFileSync } from 'node:fs';
import { describe, expect, test } from 'bun:test';

const readSource = (url: URL) => readFileSync(url, 'utf8');

describe('conversation capability header buttons', () => {
  test('share a theme-aware active style with an accent-backed surface and border', () => {
    const css = readSource(new URL('./ChatLayout/chat-layout.css', import.meta.url));
    const capabilityCss = css.slice(
      css.indexOf('/* Desktop session capability pills'),
      css.indexOf('/* Mobile: conversation header pills */')
    );

    expect(css.includes('.capability-header-btn--active')).toBe(true);
    expect(css.includes('--capability-accent')).toBe(true);
    expect(css.includes('color-mix(in srgb, var(--capability-accent)')).toBe(true);
    expect(css.includes('border-color: color-mix(in srgb, var(--capability-accent)')).toBe(true);
    expect(css.includes('[data-theme=')).toBe(true);
    expect(capabilityCss.includes('box-shadow')).toBe(false);
    expect(capabilityCss.includes('.capability-header-btn--active::before')).toBe(false);
  });

  test('all desktop header capability triggers opt into the shared active style', () => {
    const autoWork = readSource(new URL('./AutoWorkControl.tsx', import.meta.url));
    const idmm = readSource(new URL('./IdmmControl.tsx', import.meta.url));
    const knowledge = readSource(new URL('./KnowledgeControl.tsx', import.meta.url));

    for (const source of [autoWork, idmm, knowledge]) {
      expect(source.includes("from './CapabilityHeaderButton'")).toBe(true);
      expect(source.includes('capabilityHeaderButtonClass(')).toBe(true);
      expect(source.includes('capabilityHeaderButtonStyle(dotColor)')).toBe(true);
    }

    expect(autoWork.includes('capabilityHeaderButtonClass(enabled')).toBe(true);
    expect(idmm.includes('capabilityHeaderButtonClass(enabled')).toBe(true);
    expect(knowledge.includes('capabilityHeaderButtonClass(binding.enabled')).toBe(true);
  });
});
