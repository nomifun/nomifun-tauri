import { describe, expect, test } from 'bun:test';

import {
  acpTurnReducer,
  initialAcpTurnState,
  isAcpTurnBusy,
  type AcpTurnEvent,
  type AcpTurnState,
} from './acpTurnState';

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
      turnId: 'msg_1',
      processingStartedAt: 456,
    });

    expect(s.phase).toBe('starting');
    expect(s.turnId).toBe('msg_1');
    expect(s.processingStartedAt).toBe(456);
    expect(isAcpTurnBusy(s)).toBe(true);
  });

  test('thinking and content keep the turn busy', () => {
    const thinking = run([{ type: 'submit' }, { type: 'turnStarted' }, { type: 'thinking' }]);
    expect(thinking.phase).toBe('thinking');
    expect(isAcpTurnBusy(thinking)).toBe(true);

    const content = acpTurnReducer(thinking, { type: 'content' });
    expect(content.phase).toBe('streaming');
    expect(isAcpTurnBusy(content)).toBe(true);
  });

  test('permission and tooling keep the turn busy', () => {
    const permission = run([{ type: 'turnStarted' }, { type: 'permission' }]);
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
