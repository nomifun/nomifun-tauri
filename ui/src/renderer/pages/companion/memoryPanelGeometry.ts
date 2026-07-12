import type { GeomRect, GeomSize } from './windowGeometry';

export interface MonitorLayout { id: string; bounds: GeomRect; workArea: GeomRect; scaleFactor: number }
export interface DeskRestoreLayoutInput { anchor: GeomRect; originalMonitorId: string | null; monitors: MonitorLayout[]; logicalDesk: GeomSize }
export interface DeskRestoreLayout { rect: GeomRect; monitorId: string | null; scaleFactor: number }

const overlapArea = (a: GeomRect, b: GeomRect): number => {
  const width = Math.max(0, Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x));
  const height = Math.max(0, Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y));
  return width * height;
};
const clamp = (value: number, min: number, max: number): number => Math.min(Math.max(value, min), Math.max(min, max));

export function pickHostMonitor(anchor: GeomRect, monitors: GeomRect[]): GeomRect | null {
  if (monitors.length === 0) return null;
  return monitors.reduce((best, monitor) => overlapArea(anchor, monitor) > overlapArea(anchor, best) ? monitor : best);
}

export function resolveDeskRestoreLayout(input: DeskRestoreLayoutInput): DeskRestoreLayout {
  const original = input.originalMonitorId ? input.monitors.find((monitor) => monitor.id === input.originalMonitorId) : null;
  if (original) return { rect: input.anchor, monitorId: original.id, scaleFactor: original.scaleFactor };
  const hostBounds = pickHostMonitor(input.anchor, input.monitors.map((monitor) => monitor.bounds));
  const host = hostBounds ? input.monitors.find((monitor) => monitor.bounds === hostBounds || (
    monitor.bounds.x === hostBounds.x && monitor.bounds.y === hostBounds.y && monitor.bounds.width === hostBounds.width && monitor.bounds.height === hostBounds.height
  )) : null;
  if (!host) return { rect: input.anchor, monitorId: null, scaleFactor: 1 };
  const scale = Number.isFinite(host.scaleFactor) && host.scaleFactor > 0 ? host.scaleFactor : 1;
  const width = Math.min(host.workArea.width, Math.max(1, Math.round(input.logicalDesk.width * scale)));
  const height = Math.min(host.workArea.height, Math.max(1, Math.round(input.logicalDesk.height * scale)));
  const rawX = input.anchor.x + Math.round((input.anchor.width - width) / 2);
  const rawY = input.anchor.y + input.anchor.height - height;
  return { rect: { x: clamp(rawX, host.workArea.x, host.workArea.x + host.workArea.width - width), y: clamp(rawY, host.workArea.y, host.workArea.y + host.workArea.height - height), width, height }, monitorId: host.id, scaleFactor: scale };
}
