import { describe, expect, test } from 'bun:test';
import { parseMessageId } from '@/common/types/ids';

import {
  acpTurnReducer,
  initialAcpTurnState,
  isAcpTurnBusy,
  type AcpTurnEvent,
  type AcpTurnState,
} from './acpTurnState';

const STARTED_TURN_ID = parseMessageId('0190f5fe-7c00-7a00-8000-000000000001');
const STOPPED_TURN_ID = parseMessageId('0190f5fe-7c00-7a00-8000-000000000002');

function run(events: AcpTurnEvent[], from: AcpTurnState = initialAcpTurnState): AcpTurnState {
  return events.reduce(acpTurnReducer, from);
}

describe('acpTurnReducer - turn busy lifecycle', () => {
  test('submit immediately marks the turn busy', () => {
    const s = acpTurnReducer(initialAcpTurnState, { type: 'submit', startedAt: 123 });

    expect(s.phase).toBe('waiting_first_output');
    expect(s.processingStartedAt).toBe(123);
    expect(isAcpTurnBusy(s)).toBe(true);
  });

  test('hydrate(false) does not lower locally-raised submit state', () => {
    const local = acpTurnReducer(initialAcpTurnState, { type: 'submit', startedAt: 123 });
    const hydrated = acpTurnReducer(local, { type: 'hydrate', isRunning: false });

    expect(hydrated.phase).toBe('waiting_first_output');
    expect(hydrated.processingStartedAt).toBe(123);
    expect(isAcpTurnBusy(hydrated)).toBe(true);
  });

  test('turnStarted raises authoritative backend state and keeps backend timestamp', () => {
    const s = acpTurnReducer(initialAcpTurnState, {
      type: 'turnStarted',
      turnId: STARTED_TURN_ID,
      processingStartedAt: 456,
    });

    expect(s.phase).toBe('starting');
    expect(s.turnId).toBe(STARTED_TURN_ID);
    expect(s.processingStartedAt).toBe(456);
    expect(isAcpTurnBusy(s)).toBe(true);
  });

  test('known-root stop is not revived by a late raw stream start', () => {
    const stopped = run([
      { type: 'turnStarted', turnId: STOPPED_TURN_ID, processingStartedAt: 456 },
      { type: 'reset' },
    ]);
    const afterLateRawStart = acpTurnReducer(stopped, { type: 'rawStreamStarted' });

    expect(afterLateRawStart).toEqual(initialAcpTurnState);
    expect(isAcpTurnBusy(afterLateRawStart)).toBe(false);
  });

  test('thinking and content keep the turn busy', () => {
    const thinking = run([
      { type: 'submit' },
      { type: 'turnStarted', turnId: STARTED_TURN_ID },
      { type: 'thinking' },
    ]);
    expect(thinking.phase).toBe('thinking');
    expect(isAcpTurnBusy(thinking)).toBe(true);

    const content = acpTurnReducer(thinking, { type: 'content' });
    expect(content.phase).toBe('streaming');
    expect(isAcpTurnBusy(content)).toBe(true);
  });

  test('permission and tooling keep the turn busy', () => {
    const permission = run([
      { type: 'turnStarted', turnId: STARTED_TURN_ID },
      { type: 'permission' },
    ]);
    expect(permission.phase).toBe('waiting_permission');
    expect(isAcpTurnBusy(permission)).toBe(true);

    const tooling = acpTurnReducer(permission, { type: 'tooling' });
    expect(tooling.phase).toBe('tooling');
    expect(isAcpTurnBusy(tooling)).toBe(true);
  });

  test('finish and error are terminal', () => {
    expect(acpTurnReducer(run([{ type: 'submit' }, { type: 'thinking' }]), { type: 'finish' })).toEqual(
      initialAcpTurnState
    );

    const errored = acpTurnReducer(run([{ type: 'submit' }, { type: 'tooling' }]), { type: 'error' });
    expect(errored.phase).toBe('error');
    expect(isAcpTurnBusy(errored)).toBe(false);
  });
});
