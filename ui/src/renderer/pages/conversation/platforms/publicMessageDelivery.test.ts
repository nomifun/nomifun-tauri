import { describe, expect, test } from 'bun:test';
import {
  classifyPublicMessageDelivery,
  shouldDeclareFreshTurn,
} from './publicMessageDelivery';

describe('public message delivery disposition', () => {
  test('only the atomic first-delivery winner may declare a fresh turn', () => {
    const delivery = { replayed: false, completed: false };

    expect(classifyPublicMessageDelivery(delivery)).toBe('fresh');
    expect(shouldDeclareFreshTurn(delivery)).toBe(true);
  });

  test('an accepted replay is reconciliation-only', () => {
    const delivery = { replayed: true, completed: false };

    expect(classifyPublicMessageDelivery(delivery)).toBe('replayed_in_flight');
    expect(shouldDeclareFreshTurn(delivery)).toBe(false);
  });

  test('a completed replay is authoritatively closed', () => {
    const delivery = { replayed: true, completed: true };

    expect(classifyPublicMessageDelivery(delivery)).toBe('replayed_completed');
    expect(shouldDeclareFreshTurn(delivery)).toBe(false);
  });
});
