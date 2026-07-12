import { describe, expect, it } from 'vitest';
import { chooseDetachedMemoryPanelLayout, type DetachedMonitor } from './detachedMemoryPanelGeometry';

const monitor: DetachedMonitor = {
  id: 'main',
  bounds: { x: 0, y: 0, width: 1920, height: 1080 },
  workArea: { x: 0, y: 0, width: 1720, height: 1080 },
  scaleFactor: 1,
};

const intersects = (a: { x: number; y: number; width: number; height: number }, b: typeof a) =>
  a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y;

describe('chooseDetachedMemoryPanelLayout', () => {
  it('keeps a companion in the right-side Dock area untouched', () => {
    const anchor = { x: 1660, y: 760, width: 240, height: 214 };
    const result = chooseDetachedMemoryPanelLayout({ anchor, monitors: [monitor], logicalPanel: { width: 340, height: 300 } });
    expect(result).toMatchObject({ kind: 'placed', placement: 'above', monitorId: 'main', anchorRect: anchor });
    if (result.kind !== 'placed') throw new Error('expected placement');
    expect(result.panelRect.x + result.panelRect.width).toBeLessThanOrEqual(1720);
    expect(result.panelRect.y + result.panelRect.height).toBeLessThanOrEqual(anchor.y - result.gap);
    expect(intersects(result.panelRect, anchor)).toBe(false);
  });

  for (const edge of [
    { name: 'left Dock', workArea: { x: 200, y: 0, width: 1720, height: 1080 }, anchor: { x: 20, y: 760, width: 240, height: 214 } },
    { name: 'right Dock', workArea: { x: 0, y: 0, width: 1720, height: 1080 }, anchor: { x: 1660, y: 760, width: 240, height: 214 } },
    { name: 'top taskbar', workArea: { x: 0, y: 80, width: 1920, height: 1000 }, anchor: { x: 840, y: 8, width: 240, height: 214 } },
    { name: 'bottom taskbar', workArea: { x: 0, y: 0, width: 1920, height: 980 }, anchor: { x: 840, y: 850, width: 240, height: 214 } },
  ]) {
    it(`keeps the panel inside workArea with a companion in the ${edge.name}`, () => {
      const result = chooseDetachedMemoryPanelLayout({
        anchor: edge.anchor,
        monitors: [{ ...monitor, workArea: edge.workArea }],
        logicalPanel: { width: 340, height: 300 },
      });
      expect(result.kind).toBe('placed');
      if (result.kind !== 'placed') return;
      expect(result.anchorRect).toEqual(edge.anchor);
      expect(result.panelRect.x).toBeGreaterThanOrEqual(edge.workArea.x);
      expect(result.panelRect.y).toBeGreaterThanOrEqual(edge.workArea.y);
      expect(result.panelRect.x + result.panelRect.width).toBeLessThanOrEqual(edge.workArea.x + edge.workArea.width);
      expect(result.panelRect.y + result.panelRect.height).toBeLessThanOrEqual(edge.workArea.y + edge.workArea.height);
      expect(intersects(result.panelRect, edge.anchor)).toBe(false);
    });
  }

  it('chooses the left side at the top-right edge', () => {
    const result = chooseDetachedMemoryPanelLayout({
      anchor: { x: 1660, y: 8, width: 240, height: 214 },
      monitors: [{ ...monitor, workArea: monitor.bounds }],
      logicalPanel: { width: 340, height: 300 },
    });
    expect(result).toMatchObject({ kind: 'placed', placement: 'left' });
  });

  it('uses the largest-overlap negative-coordinate monitor and its scale', () => {
    const left: DetachedMonitor = {
      id: 'left-150',
      bounds: { x: -2880, y: 0, width: 2880, height: 1800 },
      workArea: { x: -2880, y: 48, width: 2880, height: 1752 },
      scaleFactor: 1.5,
    };
    const result = chooseDetachedMemoryPanelLayout({
      anchor: { x: -500, y: 1200, width: 360, height: 321 },
      monitors: [monitor, left],
      logicalPanel: { width: 340, height: 300 },
    });
    expect(result).toMatchObject({ kind: 'placed', monitorId: 'left-150', gap: 18 });
  });

  for (const scaleFactor of [1.25, 1.5, 2]) {
    it(`scales gap and minimum readability at ${scaleFactor}`, () => {
      const result = chooseDetachedMemoryPanelLayout({
        anchor: { x: 800 * scaleFactor, y: 700 * scaleFactor, width: 240 * scaleFactor, height: 214 * scaleFactor },
        monitors: [{ id: 'scaled', bounds: { x: 0, y: 0, width: 1920 * scaleFactor, height: 1080 * scaleFactor }, workArea: { x: 0, y: 0, width: 1920 * scaleFactor, height: 1080 * scaleFactor }, scaleFactor }],
        logicalPanel: { width: 340, height: 300 },
      });
      expect(result).toMatchObject({ kind: 'placed', gap: Math.round(12 * scaleFactor) });
    });
  }

  it('keeps a maximum-size custom companion disjoint', () => {
    const anchor = { x: 700, y: 500, width: 400, height: 464 };
    const result = chooseDetachedMemoryPanelLayout({ anchor, monitors: [{ ...monitor, workArea: monitor.bounds }], logicalPanel: { width: 360, height: 320 } });
    expect(result.kind).toBe('placed');
    if (result.kind === 'placed') expect(intersects(result.panelRect, anchor)).toBe(false);
  });

  it('falls back instead of overlapping when no readable region exists', () => {
    expect(chooseDetachedMemoryPanelLayout({
      anchor: { x: 0, y: 0, width: 300, height: 300 },
      monitors: [{ id: 'tiny', bounds: { x: 0, y: 0, width: 320, height: 320 }, workArea: { x: 0, y: 0, width: 320, height: 320 }, scaleFactor: 1 }],
      logicalPanel: { width: 340, height: 300 },
    })).toEqual({ kind: 'fallback', reason: 'insufficient-space' });
  });

  it('falls back when no monitor exists', () => {
    expect(chooseDetachedMemoryPanelLayout({ anchor: { x: 0, y: 0, width: 240, height: 214 }, monitors: [], logicalPanel: { width: 340, height: 300 } })).toEqual({ kind: 'fallback', reason: 'no-monitor' });
  });
});
