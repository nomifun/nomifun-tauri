import { describe, expect, test } from 'bun:test';
import { CANONICAL_UUID_V7, parseCronJobId } from '@/common/types/ids';
import {
  claimCronRunNowDelivery,
  completeCronRunNowDelivery,
  persistCronRunNowDelivery,
  readCronRunNowDelivery,
  releaseCronRunNowDelivery,
} from './cronRunNowDelivery';

const CRON_JOB_ID = parseCronJobId('0190f5fe-7c00-7a00-8000-000000000010');

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

describe('Cron run-now durable UI intent', () => {
  test('reuses the same UUIDv7 after a lost response or component remount', () => {
    const storage = createStorage();
    const first = persistCronRunNowDelivery(storage, CRON_JOB_ID);
    releaseCronRunNowDelivery(CRON_JOB_ID);

    const remounted = readCronRunNowDelivery(storage, CRON_JOB_ID);
    const retried = persistCronRunNowDelivery(storage, CRON_JOB_ID);

    expect(CANONICAL_UUID_V7.test(first.idempotency_key)).toBe(true);
    expect(remounted).toEqual(first);
    expect(retried).toEqual(first);
  });

  test('keeps failures pending and creates a new key only after acceptance', () => {
    const storage = createStorage();
    const first = persistCronRunNowDelivery(storage, CRON_JOB_ID);

    expect(claimCronRunNowDelivery(CRON_JOB_ID)).toBe(true);
    expect(claimCronRunNowDelivery(CRON_JOB_ID)).toBe(false);
    releaseCronRunNowDelivery(CRON_JOB_ID);
    expect(readCronRunNowDelivery(storage, CRON_JOB_ID)).toEqual(first);

    expect(claimCronRunNowDelivery(CRON_JOB_ID)).toBe(true);
    expect(
      completeCronRunNowDelivery(storage, CRON_JOB_ID, first.idempotency_key)
    ).toBe(true);
    expect(readCronRunNowDelivery(storage, CRON_JOB_ID)).toBeNull();

    const second = persistCronRunNowDelivery(storage, CRON_JOB_ID);
    expect(second.idempotency_key).not.toBe(first.idempotency_key);
  });
});
