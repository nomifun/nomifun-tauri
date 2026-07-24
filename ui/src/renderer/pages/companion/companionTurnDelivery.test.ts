import { describe, expect, test } from 'bun:test';
import {
  CANONICAL_UUID_V7,
  parseConversationId,
} from '@/common/types/ids';
import {
  claimCompanionTurnDelivery,
  completeCompanionTurnDelivery,
  persistCompanionTurnDelivery,
  quarantineCompanionTurnDelivery,
  readCompanionTurnDelivery,
  releaseCompanionTurnDelivery,
} from './companionTurnDelivery';

const CONVERSATION_A = parseConversationId('0190f5fe-7c00-7a00-8000-000000000401');
const CONVERSATION_B = parseConversationId('0190f5fe-7c00-7a00-8000-000000000402');

const createStorage = () => {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  };
};

describe('companion turn durable delivery identity', () => {
  test('persists one UUIDv7 before delivery and reuses it after a remount', () => {
    const storage = createStorage();
    const first = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_A,
      'hello',
      ['image.png']
    );
    const restored = readCompanionTurnDelivery(storage, 'turn');
    const secondClick = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_A,
      'new text',
      []
    );

    expect(CANONICAL_UUID_V7.test(first.idempotency_key)).toBe(true);
    expect(first.conversation_id).toBe(CONVERSATION_A);
    expect(restored).toEqual(first);
    expect(secondClick).toEqual(first);
  });

  test('retains the same payload after failure and consumes it only after acceptance', () => {
    const storage = createStorage();
    const delivery = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_A,
      'hello',
      []
    );

    expect(claimCompanionTurnDelivery('turn')).toBe(true);
    expect(claimCompanionTurnDelivery('turn')).toBe(false);
    releaseCompanionTurnDelivery('turn');
    expect(readCompanionTurnDelivery(storage, 'turn')).toEqual(delivery);

    expect(claimCompanionTurnDelivery('turn')).toBe(true);
    expect(
      completeCompanionTurnDelivery(
        storage,
        'turn',
        CONVERSATION_A,
        delivery.idempotency_key
      )
    ).toBe(true);
    expect(readCompanionTurnDelivery(storage, 'turn')).toBeNull();
  });

  test('an older response cannot consume a replacement delivery', () => {
    const storage = createStorage();
    const oldDelivery = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_A,
      'old',
      []
    );
    storage.removeItem('turn');
    const replacement = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_B,
      'new',
      []
    );

    expect(claimCompanionTurnDelivery('turn')).toBe(true);
    expect(
      completeCompanionTurnDelivery(
        storage,
        'turn',
        CONVERSATION_A,
        oldDelivery.idempotency_key
      )
    ).toBe(false);
    expect(readCompanionTurnDelivery(storage, 'turn')).toEqual(replacement);
    releaseCompanionTurnDelivery('turn');
  });

  test('quarantines legacy and successor-mismatched delivery ownership', () => {
    const storage = createStorage();
    storage.setItem(
      'turn',
      JSON.stringify({
        input: 'legacy',
        files: [],
        idempotency_key: '0190f5fe-7c00-7a00-8000-000000000499',
      })
    );

    expect(readCompanionTurnDelivery(storage, 'turn')).toBeNull();
    expect(storage.getItem('turn')).toBeNull();

    const oldDelivery = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_A,
      'old',
      []
    );
    expect(
      quarantineCompanionTurnDelivery(storage, 'turn', {
        ...oldDelivery,
        conversation_id: CONVERSATION_B,
      })
    ).toBe(false);
    expect(readCompanionTurnDelivery(storage, 'turn')).toEqual(oldDelivery);
    expect(quarantineCompanionTurnDelivery(storage, 'turn', oldDelivery)).toBe(true);
    expect(storage.getItem('turn')).toBeNull();
  });

  test('an explicit successor click mints a new key instead of migrating the old payload', () => {
    const storage = createStorage();
    const oldDelivery = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_A,
      'old payload',
      []
    );
    const successor = persistCompanionTurnDelivery(
      storage,
      'turn',
      CONVERSATION_B,
      'explicit successor payload',
      []
    );

    expect(successor.conversation_id).toBe(CONVERSATION_B);
    expect(successor.input).toBe('explicit successor payload');
    expect(successor.idempotency_key).not.toBe(oldDelivery.idempotency_key);
  });
});
