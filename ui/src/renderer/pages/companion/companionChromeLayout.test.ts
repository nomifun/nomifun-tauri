import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

const companionSource = readFileSync(new URL('./index.tsx', import.meta.url), 'utf8');
const companionCss = readFileSync(new URL('./companion.css', import.meta.url), 'utf8');
const controllerSource = readFileSync(new URL('./useDetachedMemoryPanel.ts', import.meta.url), 'utf8');
const capturePolicySource = readFileSync(new URL('./companionCapturePolicy.ts', import.meta.url), 'utf8');
const memoryGeometrySource = readFileSync(new URL('./memoryPanelGeometry.ts', import.meta.url), 'utf8');

describe('desktop companion chrome layout', () => {
  test('keeps the unread badge anchored to the figure stage', () => {
    const stageIndex = companionSource.indexOf("className='nomi-companion-stage'");
    const badgeIndex = companionSource.indexOf("className='nomi-companion-badge'");
    const figureIndex = companionSource.indexOf('ref={figureHitRef}');
    expect(stageIndex).toBeGreaterThan(-1);
    expect(badgeIndex).toBeGreaterThan(stageIndex);
    expect(figureIndex).toBeGreaterThan(badgeIndex);
  });

  test('uses a detached controller instead of shared memory expansion', () => {
    expect(companionSource.includes('useDetachedMemoryPanel')).toBe(true);
    expect(companionSource.includes("showSuggestions ? 'memory'")).toBe(false);
    expect(companionSource.includes("type ExpandedWindowMode = 'chat'")).toBe(true);
    expect(companionSource.includes("id='nomi-companion-memory-panel'")).toBe(false);
    expect(companionSource.includes('aria-expanded={memoryPanel.isExpanded}')).toBe(true);
    expect(companionSource.includes('memoryPanel.toggle')).toBe(true);
  });

  test('never resizes or repositions the companion for memory placement', () => {
    expect(controllerSource.includes('chooseDetachedMemoryPanelLayout')).toBe(true);
    expect(controllerSource.includes('monitor.workArea.position')).toBe(true);
    expect(controllerSource.includes('monitor.position.x')).toBe(true);
    expect(controllerSource.includes('onMoved')).toBe(true);
    expect(controllerSource.includes('onResized')).toBe(true);
    expect(controllerSource.includes('onScaleChanged')).toBe(true);
    expect(controllerSource.includes('memoryPanelToggleIntent')).toBe(true);
    expect(controllerSource.includes('shouldCloseMemoryPanelForOwnerGeometryChange')).toBe(true);
    expect(controllerSource.includes("close('owner-invalid')")).toBe(true);
    expect(controllerSource.includes("current.phase !== 'open'")).toBe(true);
    expect(controllerSource.includes('MEMORY_PANEL_EVENTS.snapshot')).toBe(true);
    expect(controllerSource.includes('stateRef.current = memoryPanelReducer')).toBe(true);
    expect(controllerSource.includes("current.phase !== 'preparing'")).toBe(true);
    expect(controllerSource.includes("stateRef.current.phase !== 'opening'")).toBe(true);
    expect(controllerSource.includes('finally')).toBe(true);
    expect(controllerSource.includes('.setSize(')).toBe(false);
    expect(controllerSource.includes('.setPosition(')).toBe(false);
  });

  test('retains shared native expansion only for chat surfaces', () => {
    expect(companionSource.includes('expandedWindowSessionRef')).toBe(true);
    expect(companionSource.includes('syncExpandedWindow(expandedMode)')).toBe(true);
    expect(companionSource.includes('internalWindowLayoutRef.current || expandedWindowSessionRef.current')).toBe(true);
    expect(companionSource.includes('MAX_WINDOW_RESTORE_RETRIES')).toBe(true);
  });

  test('removes the in-window suggestion surface', () => {
    expect(companionCss.includes('.nomi-companion-suggestions')).toBe(false);
    expect(companionCss.includes('is-memory-panel-open')).toBe(false);
    expect(capturePolicySource.includes('showSuggestions')).toBe(false);
    expect(memoryGeometrySource.includes('chooseMemoryPanelLayout')).toBe(false);
    expect(memoryGeometrySource.includes('fitMemoryPanelInAchievedWindow')).toBe(false);
    expect(memoryGeometrySource.includes('memoryPanelStageShiftX')).toBe(false);
    expect(memoryGeometrySource.includes('resolveDeskRestoreLayout')).toBe(true);
  });
});
