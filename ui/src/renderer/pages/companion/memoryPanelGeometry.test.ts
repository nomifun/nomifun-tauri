import { describe, expect, it } from 'vitest';
import { pickHostMonitor, resolveDeskRestoreLayout, type MonitorLayout } from './memoryPanelGeometry';

const MONITOR = { x: 0, y: 0, width: 1920, height: 1080 };

describe('pickHostMonitor', () => {
  it('picks the monitor with the largest overlap, including negative coordinates', () => {
    const left = { x: -1920, y: 0, width: 1920, height: 1080 };
    expect(pickHostMonitor({ x: -400, y: 600, width: 240, height: 214 }, [MONITOR, left])).toEqual(left);
  });
  it('falls back to the first monitor and handles an empty list', () => {
    expect(pickHostMonitor({ x: 5000, y: 5000, width: 240, height: 214 }, [MONITOR])).toEqual(MONITOR);
    expect(pickHostMonitor({ x: 0, y: 0, width: 1, height: 1 }, [])).toBeNull();
  });
});

describe('resolveDeskRestoreLayout', () => {
  const original: MonitorLayout = { id: 'external', bounds: { x: 1920, y: 0, width: 1920, height: 1080 }, workArea: { x: 1920, y: 24, width: 1920, height: 1016 }, scaleFactor: 1 };
  it('restores the exact captured rectangle while the original monitor exists', () => {
    const anchor = { x: 2500, y: 700, width: 240, height: 214 };
    expect(resolveDeskRestoreLayout({ anchor, originalMonitorId: original.id, monitors: [original], logicalDesk: { width: 240, height: 214 } }).rect).toEqual(anchor);
  });
  it('moves into a remaining monitor work area and adopts its scale', () => {
    const remaining: MonitorLayout = { id: 'retina', bounds: { x: 0, y: 0, width: 1800, height: 1168 }, workArea: { x: 0, y: 48, width: 1800, height: 1080 }, scaleFactor: 2 };
    const result = resolveDeskRestoreLayout({ anchor: { x: 2500, y: 700, width: 240, height: 214 }, originalMonitorId: original.id, monitors: [remaining], logicalDesk: { width: 240, height: 214 } });
    expect(result).toMatchObject({ monitorId: 'retina', scaleFactor: 2, rect: { width: 480, height: 428 } });
  });
});
