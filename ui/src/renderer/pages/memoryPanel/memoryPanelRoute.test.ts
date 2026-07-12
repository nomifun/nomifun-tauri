import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const routerSource = readFileSync(new URL('../../components/layout/Router.tsx', import.meta.url), 'utf8');
const panelSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
const panelCss = readFileSync(new URL('./memoryPanel.css', import.meta.url), 'utf8');
const nativeWindowSource = readFileSync(new URL('../../../../../apps/desktop/src/memory_panel_window.rs', import.meta.url), 'utf8');
const bodyRule = panelCss.match(/^\.nomi-memory-panel__body\s*\{([^}]*)\}/m)?.[1] ?? '';
const titleRule = panelCss.match(/^\.nomi-memory-panel__title\s*\{([^}]*)\}/m)?.[1] ?? '';

describe('detached memory panel route', () => {
  test('is standalone, focus-aware, semantic and scrollable', () => {
    expect(routerSource.includes("path='/nomi-memory-panel'")).toBe(true);
    expect(panelSource.includes('onFocusChanged')).toBe(true);
    expect(panelSource.includes("phaseRef.current !== 'open'")).toBe(true);
    expect(panelSource.includes('activationPendingRef.current')).toBe(true);
    expect(panelSource.includes('snapshotRef.current = payload')).toBe(true);
    expect(panelSource.includes("snapshotRef.current?.requestId !== payload.requestId")).toBe(true);
    expect(panelSource.includes("reason: 'owner-invalid'")).toBe(true);
    expect(panelSource.includes('hideMemoryPanelWindow(previous.requestId)')).toBe(true);
    expect(panelSource.includes('firstItemRef.current?.focus')).toBe(true);
    expect(panelSource.includes("sameRequest ? phaseRef.current : 'preparing'")).toBe(true);
    expect(panelSource.includes("phaseRef.current !== 'opening'")).toBe(true);
    expect(panelSource.includes('MEMORY_PANEL_EVENTS.activate')).toBe(true);
    expect(panelSource.includes("role='dialog'")).toBe(true);
    expect(panelCss.includes('overflow-y: auto')).toBe(true);
    expect(panelCss.includes('min(320px, 100vh)')).toBe(true);
    expect(panelCss.includes('min(120px, 100vh)')).toBe(true);
    expect(panelCss.includes('overflow-wrap: anywhere')).toBe(true);
    expect(panelCss.includes('@media (prefers-reduced-motion: reduce)')).toBe(true);
    expect(nativeWindowSource.includes('.inner_size(300.0, 320.0)')).toBe(true);
    expect(bodyRule.includes('display: -webkit-box')).toBe(true);
    expect(bodyRule.includes('-webkit-box-orient: vertical')).toBe(true);
    expect(bodyRule.includes('-webkit-line-clamp: 2')).toBe(true);
    expect(bodyRule.includes('overflow: hidden')).toBe(true);
    expect(titleRule.includes('-webkit-line-clamp')).toBe(false);
    expect(panelCss.includes('backdrop-filter')).toBe(false);
  });
});
