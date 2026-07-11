import type { GeomRect, GeomSize } from './windowGeometry';

export type DetachedMemoryPanelPlacement = 'above' | 'left' | 'right';

export interface DetachedMonitor {
  id: string;
  bounds: GeomRect;
  workArea: GeomRect;
  scaleFactor: number;
}

export interface DetachedMemoryPanelInput {
  anchor: GeomRect;
  monitors: DetachedMonitor[];
  logicalPanel: GeomSize;
  logicalMinimum?: GeomSize;
  logicalGap?: number;
}

export type DetachedMemoryPanelResult =
  | { kind: 'placed'; placement: DetachedMemoryPanelPlacement; panelRect: GeomRect; anchorRect: GeomRect; monitorId: string; scaleFactor: number; gap: number }
  | { kind: 'fallback'; reason: 'no-monitor' | 'insufficient-space' };

const overlapArea = (a: GeomRect, b: GeomRect) =>
  Math.max(0, Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x)) *
  Math.max(0, Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y));
const clamp = (value: number, min: number, max: number) => Math.min(Math.max(value, min), Math.max(min, max));
const intersects = (a: GeomRect, b: GeomRect) =>
  a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y;

export function chooseDetachedMemoryPanelLayout(input: DetachedMemoryPanelInput): DetachedMemoryPanelResult {
  if (input.monitors.length === 0) return { kind: 'fallback', reason: 'no-monitor' };
  const host = input.monitors.reduce((best, monitor) =>
    overlapArea(input.anchor, monitor.bounds) > overlapArea(input.anchor, best.bounds) ? monitor : best
  );
  const scale = Number.isFinite(host.scaleFactor) && host.scaleFactor > 0 ? host.scaleFactor : 1;
  const desired = { width: Math.round(input.logicalPanel.width * scale), height: Math.round(input.logicalPanel.height * scale) };
  const minimumLogical = input.logicalMinimum ?? { width: 280, height: 120 };
  const minimum = { width: Math.round(minimumLogical.width * scale), height: Math.round(minimumLogical.height * scale) };
  const gap = Math.round((input.logicalGap ?? 12) * scale);
  const work = host.workArea;
  const candidates: Array<{ placement: DetachedMemoryPanelPlacement; rect: GeomRect; full: boolean; area: number }> = [];

  const add = (placement: DetachedMemoryPanelPlacement, rect: GeomRect) => {
    if (rect.width < minimum.width || rect.height < minimum.height || intersects(rect, input.anchor)) return;
    if (rect.x < work.x || rect.y < work.y || rect.x + rect.width > work.x + work.width || rect.y + rect.height > work.y + work.height) return;
    candidates.push({ placement, rect, full: rect.width >= desired.width && rect.height >= desired.height, area: rect.width * rect.height });
  };

  const aboveWidth = Math.min(desired.width, work.width);
  const aboveHeight = Math.min(desired.height, Math.max(0, input.anchor.y - gap - work.y), work.height);
  if (aboveWidth > 0 && aboveHeight > 0) {
    add('above', {
      x: clamp(Math.round(input.anchor.x + (input.anchor.width - aboveWidth) / 2), work.x, work.x + work.width - aboveWidth),
      y: clamp(input.anchor.y - gap - aboveHeight, work.y, work.y + work.height - aboveHeight),
      width: aboveWidth,
      height: aboveHeight,
    });
  }

  const sideHeight = Math.min(desired.height, work.height);
  const leftWidth = Math.min(desired.width, work.width, Math.max(0, input.anchor.x - gap - work.x));
  if (leftWidth > 0 && sideHeight > 0) {
    add('left', {
      x: clamp(input.anchor.x - gap - leftWidth, work.x, work.x + work.width - leftWidth),
      y: clamp(Math.round(input.anchor.y + (input.anchor.height - sideHeight) / 2), work.y, work.y + work.height - sideHeight),
      width: leftWidth,
      height: sideHeight,
    });
  }
  const rightStart = Math.max(work.x, input.anchor.x + input.anchor.width + gap);
  const rightWidth = Math.min(desired.width, Math.max(0, work.x + work.width - rightStart));
  if (rightWidth > 0 && sideHeight > 0) {
    add('right', {
      x: rightStart,
      y: clamp(Math.round(input.anchor.y + (input.anchor.height - sideHeight) / 2), work.y, work.y + work.height - sideHeight),
      width: rightWidth,
      height: sideHeight,
    });
  }

  const fullAbove = candidates.find((candidate) => candidate.placement === 'above' && candidate.full);
  const fullSides = candidates.filter((candidate) => candidate.placement !== 'above' && candidate.full).sort((a, b) => b.area - a.area);
  const selected = fullAbove ?? fullSides[0] ?? candidates.sort((a, b) => b.area - a.area)[0];
  if (!selected) return { kind: 'fallback', reason: 'insufficient-space' };
  return { kind: 'placed', placement: selected.placement, panelRect: selected.rect, anchorRect: input.anchor, monitorId: host.id, scaleFactor: scale, gap };
}
