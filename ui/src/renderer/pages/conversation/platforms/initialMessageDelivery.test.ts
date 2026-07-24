import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId } from '@/common/types/ids';
import {
  claimInitialMessageDelivery,
  completeInitialMessageDelivery,
  handleInitialMessageDeliveryFailure,
  persistInitialMessageDelivery,
  readAuthorizedInitialMessageDelivery,
  readInitialMessageDelivery,
  releaseInitialMessageDelivery,
} from './initialMessageDelivery';

const CONVERSATION_ID = parseConversationId('0190f5fe-7c00-7a00-8000-000000000301');

const createStorage = () => {
  const values = new Map<string, string>();
  return {
    values,
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  };
};

describe('initial message durable delivery identity', () => {
  test('quarantines a legacy payload instead of assigning it a new delivery identity', () => {
    const storage = createStorage();
    storage.setItem('initial', JSON.stringify({ input: 'hello', files: ['a.txt'] }));

    expect(readInitialMessageDelivery(storage, 'initial')).toBeNull();
    expect(storage.getItem('initial')).toBeNull();
  });

  test('authorizes an exact Pending conversation only while its transcript is empty', async () => {
    const storage = createStorage();
    const delivery = {
      conversation_id: CONVERSATION_ID,
      initial_admission_epoch: 0 as const,
      input: 'hello',
      files: [],
      idempotency_key: 'stable-delivery-key',
    };
    storage.setItem('initial', JSON.stringify(delivery));

    const result = await readAuthorizedInitialMessageDelivery(
      storage,
      'initial',
      CONVERSATION_ID,
      {
        getConversation: async () =>
          ({
            id: CONVERSATION_ID,
            status: 'pending',
            runtime: { active_turn_id: undefined, is_processing: false },
          }) as TChatConversation,
        getTranscriptSummary: async () => ({ items: [], total: 0 }),
      }
    );

    expect(result).toEqual(delivery);
    expect(storage.getItem('initial')).not.toBeNull();
  });

  test('quarantines a Finished persisted Guid payload before any POST can start', async () => {
    const storage = createStorage();
    storage.setItem(
      'initial',
      JSON.stringify({
        conversation_id: CONVERSATION_ID,
        initial_admission_epoch: 0,
        input: 'must not restart',
        files: [],
        idempotency_key: 'finished-delivery-key',
      })
    );
    let transcriptReads = 0;

    const result = await readAuthorizedInitialMessageDelivery(
      storage,
      'initial',
      CONVERSATION_ID,
      {
        getConversation: async () =>
          ({ id: CONVERSATION_ID, status: 'finished' }) as TChatConversation,
        getTranscriptSummary: async () => {
          transcriptReads += 1;
          return { items: [{}], total: 1 };
        },
      }
    );

    expect(result).toBeNull();
    expect(transcriptReads).toBe(0);
    expect(storage.getItem('initial')).toBeNull();
  });

  test('quarantines Pending payloads with history and unknown authority', async () => {
    const storage = createStorage();
    const persisted = JSON.stringify({
      conversation_id: CONVERSATION_ID,
      initial_admission_epoch: 0,
      input: 'stale',
      files: [],
      idempotency_key: 'stale-delivery-key',
    });
    storage.setItem('history', persisted);

    expect(
      await readAuthorizedInitialMessageDelivery(
        storage,
        'history',
        CONVERSATION_ID,
        {
          getConversation: async () =>
            ({ id: CONVERSATION_ID, status: 'pending' }) as TChatConversation,
          getTranscriptSummary: async () => ({ items: [{}], total: 1 }),
        }
      )
    ).toBeNull();
    expect(storage.getItem('history')).toBeNull();

    storage.setItem('unknown', persisted);
    expect(
      await readAuthorizedInitialMessageDelivery(
        storage,
        'unknown',
        CONVERSATION_ID,
        {
          getConversation: async () => {
            throw new Error('authority unavailable');
          },
          getTranscriptSummary: async () => ({ items: [], total: 0 }),
        }
      )
    ).toBeNull();
    expect(storage.getItem('unknown')).toBeNull();
  });

  test('keeps the payload through failure and consumes it only after acceptance', () => {
    const storage = createStorage();
    storage.setItem(
      'initial',
      JSON.stringify({
        conversation_id: CONVERSATION_ID,
        initial_admission_epoch: 0,
        input: 'hello',
        files: [],
        idempotency_key: 'stable-delivery-key',
      })
    );

    expect(claimInitialMessageDelivery('initial')).toBe(true);
    expect(claimInitialMessageDelivery('initial')).toBe(false);
    releaseInitialMessageDelivery('initial');
    expect(storage.getItem('initial')).not.toBeNull();

    expect(claimInitialMessageDelivery('initial')).toBe(true);
    completeInitialMessageDelivery(storage, 'initial', 'stable-delivery-key');
    expect(storage.getItem('initial')).toBeNull();
    expect(claimInitialMessageDelivery('initial')).toBe(true);
    releaseInitialMessageDelivery('initial');
  });

  test('an older accepted response cannot consume a newer handoff at the same key', () => {
    const storage = createStorage();
    storage.setItem(
      'initial',
      JSON.stringify({
        conversation_id: CONVERSATION_ID,
        initial_admission_epoch: 0,
        input: 'new',
        files: [],
        idempotency_key: 'new-delivery-key',
      })
    );
    expect(claimInitialMessageDelivery('initial')).toBe(true);

    expect(completeInitialMessageDelivery(storage, 'initial', 'old-delivery-key')).toBe(false);
    expect(JSON.parse(storage.getItem('initial') ?? '{}').idempotency_key).toBe('new-delivery-key');
    expect(claimInitialMessageDelivery('initial')).toBe(true);
    releaseInitialMessageDelivery('initial');
  });

  test('quarantines an exact initial-only 409 but retains transport failures', () => {
    const storage = createStorage();
    const first = persistInitialMessageDelivery(
      storage,
      'conflict',
      CONVERSATION_ID,
      'stale after reset',
      []
    );
    expect(claimInitialMessageDelivery('conflict')).toBe(true);
    handleInitialMessageDeliveryFailure(
      storage,
      'conflict',
      first.idempotency_key,
      { name: 'BackendHttpError', status: 409, code: 'CONFLICT' }
    );
    expect(storage.getItem('conflict')).toBeNull();
    expect(claimInitialMessageDelivery('conflict')).toBe(true);
    releaseInitialMessageDelivery('conflict');

    const retryable = persistInitialMessageDelivery(
      storage,
      'network',
      CONVERSATION_ID,
      'retry me',
      []
    );
    expect(claimInitialMessageDelivery('network')).toBe(true);
    handleInitialMessageDeliveryFailure(
      storage,
      'network',
      retryable.idempotency_key,
      new Error('network unavailable')
    );
    expect(readInitialMessageDelivery(storage, 'network')).toEqual(retryable);
    expect(claimInitialMessageDelivery('network')).toBe(true);
    releaseInitialMessageDelivery('network');
  });

  test('a repeated automatic trigger keeps the unresolved persisted identity', () => {
    const storage = createStorage();
    const first = persistInitialMessageDelivery(
      storage,
      'automatic',
      CONVERSATION_ID,
      'install',
      []
    );
    const repeated = persistInitialMessageDelivery(
      storage,
      'automatic',
      CONVERSATION_ID,
      'diagnose',
      []
    );

    expect(repeated).toEqual(first);
    expect(JSON.parse(storage.getItem('automatic') ?? '{}')).toEqual(first);
  });
});
