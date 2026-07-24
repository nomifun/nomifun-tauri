import {
  CANONICAL_UUID_V7,
  parseConversationId,
  type ConversationId,
} from '@/common/types/ids';
import { uuidv7 } from '@/common/utils/uuidv7';

export type PersistedCompanionTurnDelivery = {
  /** Exact durable Conversation that accepted ownership when the user clicked. */
  conversation_id: ConversationId;
  input: string;
  files: string[];
  idempotency_key: string;
};

type CompanionTurnStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

const deliveriesInFlight = new Set<string>();

const parseDelivery = (stored: string): PersistedCompanionTurnDelivery | null => {
  try {
    const parsed = JSON.parse(stored) as unknown;
    if (!parsed || typeof parsed !== 'object') return null;

    const candidate = parsed as Record<string, unknown>;
    if (
      typeof candidate.input !== 'string' ||
      !Array.isArray(candidate.files) ||
      !candidate.files.every((file) => typeof file === 'string') ||
      typeof candidate.conversation_id !== 'string' ||
      typeof candidate.idempotency_key !== 'string' ||
      !CANONICAL_UUID_V7.test(candidate.idempotency_key)
    ) {
      return null;
    }

    const conversation_id = parseConversationId(candidate.conversation_id);
    return {
      conversation_id,
      input: candidate.input,
      files: [...candidate.files],
      idempotency_key: candidate.idempotency_key,
    };
  } catch {
    return null;
  }
};

export const readCompanionTurnDelivery = (
  storage: CompanionTurnStorage,
  storageKey: string
): PersistedCompanionTurnDelivery | null => {
  const stored = storage.getItem(storageKey);
  if (!stored) return null;
  const delivery = parseDelivery(stored);
  if (!delivery) storage.removeItem(storageKey);
  return delivery;
};

/**
 * Persist a local companion command before its POST starts.
 *
 * A still-pending record always wins over a newer click. This prevents an
 * accepted response that was lost in transit from being overwritten with a
 * different delivery identity before it can be replayed.
 */
export const persistCompanionTurnDelivery = (
  storage: CompanionTurnStorage,
  storageKey: string,
  conversationId: ConversationId,
  input: string,
  files: string[]
): PersistedCompanionTurnDelivery => {
  const pending = readCompanionTurnDelivery(storage, storageKey);
  if (pending?.conversation_id === conversationId) return pending;
  if (pending) storage.removeItem(storageKey);

  const delivery: PersistedCompanionTurnDelivery = {
    conversation_id: conversationId,
    input,
    files: [...files],
    idempotency_key: uuidv7(),
  };
  storage.setItem(storageKey, JSON.stringify(delivery));
  return delivery;
};

/** Prevent StrictMode/remount overlap while allowing a later same-key replay. */
export const claimCompanionTurnDelivery = (storageKey: string): boolean => {
  if (deliveriesInFlight.has(storageKey)) return false;
  deliveriesInFlight.add(storageKey);
  return true;
};

export const releaseCompanionTurnDelivery = (storageKey: string): void => {
  deliveriesInFlight.delete(storageKey);
};

/** Quarantine only the exact stale operation inspected by the caller. */
export const quarantineCompanionTurnDelivery = (
  storage: CompanionTurnStorage,
  storageKey: string,
  delivery: PersistedCompanionTurnDelivery
): boolean => {
  const current = readCompanionTurnDelivery(storage, storageKey);
  if (
    current?.conversation_id !== delivery.conversation_id ||
    current.idempotency_key !== delivery.idempotency_key
  ) {
    return false;
  }
  storage.removeItem(storageKey);
  releaseCompanionTurnDelivery(storageKey);
  return true;
};

/** Remove only the exact delivery whose POST returned an accepted response. */
export const completeCompanionTurnDelivery = (
  storage: CompanionTurnStorage,
  storageKey: string,
  acceptedConversationId: ConversationId,
  acceptedIdempotencyKey: string
): boolean => {
  let consumed = false;
  const current = readCompanionTurnDelivery(storage, storageKey);
  if (
    current?.conversation_id === acceptedConversationId &&
    current.idempotency_key === acceptedIdempotencyKey
  ) {
    storage.removeItem(storageKey);
    consumed = true;
  }
  releaseCompanionTurnDelivery(storageKey);
  return consumed;
};
