import { describe, expect, it } from 'vitest';
import {
  initialMemoryPanelState,
  memoryPanelReducer,
  memoryPanelToggleIntent,
  shouldCloseMemoryPanelForOwnerGeometryChange,
} from './memoryPanelProtocol';
import { parseCompanionId } from '@/common/types/ids';

const COMPANION_A = parseCompanionId('0190f5fe-7c00-7a00-8000-000000000001');
const COMPANION_B = parseCompanionId('0190f5fe-7c00-7a00-8000-000000000002');

describe('memoryPanelReducer', () => {
  it('ignores stale close completion after a newer open', () => {
    const first = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r1', ownerCompanionId: COMPANION_A });
    const newer = memoryPanelReducer(first, { type: 'begin', requestId: 'r2', ownerCompanionId: COMPANION_B });
    expect(memoryPanelReducer(newer, { type: 'closed', requestId: 'r1' })).toEqual(newer);
  });

  it('accepts blur only after the panel is open', () => {
    const preparing = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r1', ownerCompanionId: COMPANION_A });
    expect(memoryPanelReducer(preparing, { type: 'request-close', requestId: 'r1', reason: 'blur' })).toEqual(preparing);
    const open = memoryPanelReducer(preparing, { type: 'opened', requestId: 'r1' });
    expect(memoryPanelReducer(open, { type: 'request-close', requestId: 'r1', reason: 'blur' }).phase).toBe('closing');
  });

  it('records Escape as the close reason that restores focus', () => {
    const preparing = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r1', ownerCompanionId: COMPANION_A });
    const open = memoryPanelReducer(preparing, { type: 'opened', requestId: 'r1' });
    expect(memoryPanelReducer(open, { type: 'request-close', requestId: 'r1', reason: 'escape' }).closeReason).toBe('escape');
  });

  it('ignores duplicate close requests and stale opened events', () => {
    const preparing = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r2', ownerCompanionId: COMPANION_B });
    expect(memoryPanelReducer(preparing, { type: 'opened', requestId: 'r1' })).toEqual(preparing);
    const open = memoryPanelReducer(preparing, { type: 'opened', requestId: 'r2' });
    const closing = memoryPanelReducer(open, { type: 'request-close', requestId: 'r2', reason: 'toggle' });
    expect(memoryPanelReducer(closing, { type: 'request-close', requestId: 'r2', reason: 'blur' })).toEqual(closing);
  });

  it('reopens from closing while normal active phases toggle closed', () => {
    expect(memoryPanelToggleIntent('closed')).toBe('open');
    expect(memoryPanelToggleIntent('closing')).toBe('open');
    expect(memoryPanelToggleIntent('preparing')).toBe('close');
    expect(memoryPanelToggleIntent('opening')).toBe('close');
    expect(memoryPanelToggleIntent('open')).toBe('close');

    const preparing = memoryPanelReducer(initialMemoryPanelState, { type: 'begin', requestId: 'r1', ownerCompanionId: COMPANION_A });
    const open = memoryPanelReducer(preparing, { type: 'opened', requestId: 'r1' });
    const closing = memoryPanelReducer(open, { type: 'request-close', requestId: 'r1', reason: 'blur' });
    expect(memoryPanelReducer(closing, { type: 'begin', requestId: 'r2', ownerCompanionId: COMPANION_A })).toMatchObject({
      phase: 'preparing',
      requestId: 'r2',
      ownerCompanionId: COMPANION_A,
    });
  });

  it('invalidates unstable owner geometry throughout preparation and display', () => {
    expect(shouldCloseMemoryPanelForOwnerGeometryChange('closed')).toBe(false);
    expect(shouldCloseMemoryPanelForOwnerGeometryChange('closing')).toBe(false);
    expect(shouldCloseMemoryPanelForOwnerGeometryChange('preparing')).toBe(true);
    expect(shouldCloseMemoryPanelForOwnerGeometryChange('opening')).toBe(true);
    expect(shouldCloseMemoryPanelForOwnerGeometryChange('open')).toBe(true);
  });
});
