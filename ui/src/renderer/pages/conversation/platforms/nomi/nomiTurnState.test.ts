import { describe, expect, test } from 'bun:test';

import {
  initialNomiTurnState,
  isTurnRunning,
  nomiTurnReducer,
  type NomiTurnEvent,
  type NomiTurnState,
} from './nomiTurnState';

/** Fold a sequence of events over the initial state. */
function run(events: NomiTurnEvent[], from: NomiTurnState = initialNomiTurnState): NomiTurnState {
  return events.reduce(nomiTurnReducer, from);
}

describe('nomiTurnReducer — basic transitions', () => {
  test('initial state is idle', () => {
    expect(isTurnRunning(initialNomiTurnState)).toBe(false);
  });

  test('activity raises streamRunning without touching waiting', () => {
    const s = nomiTurnReducer({ ...initialNomiTurnState, waitingResponse: true }, { type: 'activity' });
    expect(s.streamRunning).toBe(true);
    expect(s.waitingResponse).toBe(true);
  });

  test('content clears waitingResponse and runs', () => {
    const s = run([{ type: 'setWaiting', value: true }, { type: 'content' }]);
    expect(s.streamRunning).toBe(true);
    expect(s.waitingResponse).toBe(false);
    expect(isTurnRunning(s)).toBe(true);
  });

  test('complete projected content renders without starting or ending a turn', () => {
    const idle = nomiTurnReducer(initialNomiTurnState, { type: 'content', streamComplete: true });
    expect(idle).toEqual(initialNomiTurnState);

    const concurrentTurn = run([{ type: 'activity' }]);
    const unchanged = nomiTurnReducer(concurrentTurn, { type: 'content', streamComplete: true });
    expect(unchanged).toEqual(concurrentTurn);
  });

  test('setWaiting toggles only waitingResponse', () => {
    const on = nomiTurnReducer(initialNomiTurnState, { type: 'setWaiting', value: true });
    expect(on).toEqual({ streamRunning: false, hasActiveTools: false, waitingResponse: true });
    const off = nomiTurnReducer(on, { type: 'setWaiting', value: false });
    expect(off.waitingResponse).toBe(false);
  });
});

describe('nomiTurnReducer — tool groups', () => {
  test('active tools mark the turn running', () => {
    const s = nomiTurnReducer(initialNomiTurnState, { type: 'toolGroup', hasActive: true, hasAny: true });
    expect(s.hasActiveTools).toBe(true);
    expect(s.streamRunning).toBe(true);
    expect(isTurnRunning(s)).toBe(true);
  });

  test('tools going active → inactive raises waitingResponse (next model turn)', () => {
    const s = run([
      { type: 'toolGroup', hasActive: true, hasAny: true },
      { type: 'toolGroup', hasActive: false, hasAny: true },
    ]);
    expect(s.hasActiveTools).toBe(false);
    expect(s.waitingResponse).toBe(true);
    expect(isTurnRunning(s)).toBe(true);
  });

  test('an empty tool group does not spuriously raise waitingResponse', () => {
    const s = run([
      { type: 'toolGroup', hasActive: true, hasAny: true },
      { type: 'toolGroup', hasActive: false, hasAny: false },
    ]);
    expect(s.waitingResponse).toBe(false);
  });
});

describe('nomiTurnReducer — terminal events clear ALL activity (stuck-spinner fix)', () => {
  test('finish clears hasActiveTools even if a tool was still marked active', () => {
    // Regression: the old code never reset hasActiveTools on finish, so the
    // spinner stayed stuck running forever.
    const mid = run([{ type: 'toolGroup', hasActive: true, hasAny: true }]);
    expect(mid.hasActiveTools).toBe(true);
    const done = nomiTurnReducer(mid, { type: 'finish' });
    expect(done).toEqual(initialNomiTurnState);
    expect(isTurnRunning(done)).toBe(false);
  });

  test('error clears all activity', () => {
    const mid = run([{ type: 'activity' }, { type: 'toolGroup', hasActive: true, hasAny: true }]);
    const errored = nomiTurnReducer(mid, { type: 'error' });
    expect(isTurnRunning(errored)).toBe(false);
  });
});

describe('nomiTurnReducer — auto-recover after a premature finish', () => {
  test('activity after finish re-raises running (late event recovery)', () => {
    const s = run([{ type: 'finish' }, { type: 'activity' }]);
    expect(s.streamRunning).toBe(true);
  });

  test('content after finish re-raises running', () => {
    const s = run([{ type: 'finish' }, { type: 'content' }]);
    expect(isTurnRunning(s)).toBe(true);
  });
});

describe('nomiTurnReducer — hydrate is raise-only', () => {
  test('hydrate(true) raises running', () => {
    const s = nomiTurnReducer(initialNomiTurnState, { type: 'hydrate', isRunning: true });
    expect(isTurnRunning(s)).toBe(true);
  });

  test('hydrate(false) does NOT lower a locally-raised waiting flag', () => {
    // A send raised waitingResponse before the stale is_processing=false query
    // resolved; hydrate must not clobber it.
    const local = nomiTurnReducer(initialNomiTurnState, { type: 'setWaiting', value: true });
    const s = nomiTurnReducer(local, { type: 'hydrate', isRunning: false });
    expect(s.waitingResponse).toBe(true);
  });

  test('hydrate(false) on a fresh state stays idle', () => {
    const s = nomiTurnReducer(initialNomiTurnState, { type: 'hydrate', isRunning: false });
    expect(isTurnRunning(s)).toBe(false);
  });

  test('fresh authoritative idle hydration clears activity from a late prior-turn stream', () => {
    const lateActivity = run([
      { type: 'activity' },
      { type: 'toolGroup', hasActive: true, hasAny: true },
    ]);
    const s = nomiTurnReducer(lateActivity, {
      type: 'hydrate',
      isRunning: false,
      settleIdle: true,
    });

    expect(s).toEqual(initialNomiTurnState);
    expect(isTurnRunning(s)).toBe(false);
  });

  test('hydrate clears stale tool state (re-derived from messages)', () => {
    const withTools = run([{ type: 'toolGroup', hasActive: true, hasAny: true }]);
    const s = nomiTurnReducer(withTools, { type: 'hydrate', isRunning: true });
    expect(s.hasActiveTools).toBe(false);
  });
});

describe('nomiTurnReducer — reset', () => {
  test('reset returns to idle from any state', () => {
    const busy = run([
      { type: 'activity' },
      { type: 'toolGroup', hasActive: true, hasAny: true },
      { type: 'setWaiting', value: true },
    ]);
    expect(isTurnRunning(busy)).toBe(true);
    const s = nomiTurnReducer(busy, { type: 'reset' });
    expect(s).toEqual(initialNomiTurnState);
  });
});

describe('nomiTurnReducer — a representative full turn', () => {
  test('send → start → content → tool active → tool done → content → finish', () => {
    let s = initialNomiTurnState;
    s = nomiTurnReducer(s, { type: 'setWaiting', value: true }); // send
    expect(isTurnRunning(s)).toBe(true);
    s = nomiTurnReducer(s, { type: 'activity' }); // start
    s = nomiTurnReducer(s, { type: 'content' }); // first text
    expect(s.waitingResponse).toBe(false);
    s = nomiTurnReducer(s, { type: 'toolGroup', hasActive: true, hasAny: true });
    expect(s.hasActiveTools).toBe(true);
    s = nomiTurnReducer(s, { type: 'toolGroup', hasActive: false, hasAny: true });
    expect(s.waitingResponse).toBe(true); // awaiting next model turn
    s = nomiTurnReducer(s, { type: 'content' }); // model resumes
    expect(s.waitingResponse).toBe(false);
    s = nomiTurnReducer(s, { type: 'finish' });
    expect(isTurnRunning(s)).toBe(false); // cleanly idle
  });
});
