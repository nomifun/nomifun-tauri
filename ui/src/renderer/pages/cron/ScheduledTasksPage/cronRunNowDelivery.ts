import { CANONICAL_UUID_V7, type CronJobId } from '@/common/types/ids';
import { uuidv7 } from '@/common/utils/uuidv7';

export type PersistedCronRunNowDelivery = {
  cron_job_id: CronJobId;
  idempotency_key: string;
};

type CronRunNowStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

const deliveriesInFlight = new Set<string>();

export const cronRunNowStorageKey = (cronJobId: CronJobId): string =>
  `nomifun:cron-run-now:v1:${cronJobId}`;

const parseDelivery = (
  stored: string,
  expectedCronJobId: CronJobId
): PersistedCronRunNowDelivery | null => {
  try {
    const parsed = JSON.parse(stored) as unknown;
    if (!parsed || typeof parsed !== 'object') return null;
    const candidate = parsed as Record<string, unknown>;
    if (
      candidate.cron_job_id !== expectedCronJobId ||
      typeof candidate.idempotency_key !== 'string' ||
      !CANONICAL_UUID_V7.test(candidate.idempotency_key)
    ) {
      return null;
    }
    return {
      cron_job_id: expectedCronJobId,
      idempotency_key: candidate.idempotency_key,
    };
  } catch {
    return null;
  }
};

export const readCronRunNowDelivery = (
  storage: CronRunNowStorage,
  cronJobId: CronJobId
): PersistedCronRunNowDelivery | null => {
  const stored = storage.getItem(cronRunNowStorageKey(cronJobId));
  return stored ? parseDelivery(stored, cronJobId) : null;
};

/**
 * Persist one user intent before its POST starts.
 *
 * A pending record always wins. Therefore a lost response, component remount,
 * or process-local retry reuses the exact transport identity instead of
 * creating another durable Cron reservation.
 */
export const persistCronRunNowDelivery = (
  storage: CronRunNowStorage,
  cronJobId: CronJobId
): PersistedCronRunNowDelivery => {
  const pending = readCronRunNowDelivery(storage, cronJobId);
  if (pending) return pending;

  const delivery: PersistedCronRunNowDelivery = {
    cron_job_id: cronJobId,
    idempotency_key: uuidv7(),
  };
  storage.setItem(cronRunNowStorageKey(cronJobId), JSON.stringify(delivery));
  return delivery;
};

/** Prevent overlapping component instances from submitting the same intent. */
export const claimCronRunNowDelivery = (cronJobId: CronJobId): boolean => {
  const key = cronRunNowStorageKey(cronJobId);
  if (deliveriesInFlight.has(key)) return false;
  deliveriesInFlight.add(key);
  return true;
};

export const releaseCronRunNowDelivery = (cronJobId: CronJobId): void => {
  deliveriesInFlight.delete(cronRunNowStorageKey(cronJobId));
};

/** Remove only the exact intent whose HTTP response was accepted. */
export const completeCronRunNowDelivery = (
  storage: CronRunNowStorage,
  cronJobId: CronJobId,
  acceptedIdempotencyKey: string
): boolean => {
  let consumed = false;
  const current = readCronRunNowDelivery(storage, cronJobId);
  if (current?.idempotency_key === acceptedIdempotencyKey) {
    storage.removeItem(cronRunNowStorageKey(cronJobId));
    consumed = true;
  }
  releaseCronRunNowDelivery(cronJobId);
  return consumed;
};
