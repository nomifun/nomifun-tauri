import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import type { TChatConversation } from '@/common/config/storage';
import {
  parseConversationId,
  type ConversationId,
} from '@/common/types/ids';
import { uuidv7 } from '@/common/utils/uuidv7';

export type PersistedInitialMessage = {
  /** Exact newly-created Conversation that owns this one-shot handoff. */
  conversation_id: ConversationId;
  /** Initial-only backend admission is valid solely at creation generation 0. */
  initial_admission_epoch: 0;
  input: string;
  files: string[];
  idempotency_key: string;
};

type InitialMessageStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

const MAX_IDEMPOTENCY_KEY_LENGTH = 128;
const VISIBLE_ASCII = /^[\x21-\x7e]+$/;
const deliveriesInFlight = new Set<string>();

const isUsableIdempotencyKey = (value: unknown): value is string =>
  typeof value === 'string' &&
  value.length > 0 &&
  value.length <= MAX_IDEMPOTENCY_KEY_LENGTH &&
  VISIBLE_ASCII.test(value);

/**
 * Read an initial-message handoff without consuming it.
 *
 * A persisted handoff without its original delivery identity has no safe
 * replay semantics: assigning a new key would turn unknown historical state
 * into a brand-new command. Corrupt and legacy records are therefore removed
 * fail-closed instead of being migrated.
 */
export const readInitialMessageDelivery = (
  storage: InitialMessageStorage,
  storageKey: string
): PersistedInitialMessage | null => {
  const stored = storage.getItem(storageKey);
  if (!stored) return null;

  try {
    const parsed = JSON.parse(stored) as unknown;
    if (!parsed || typeof parsed !== 'object') {
      storage.removeItem(storageKey);
      return null;
    }

    const candidate = parsed as Record<string, unknown>;
    if (
      typeof candidate.input !== 'string' ||
      typeof candidate.conversation_id !== 'string' ||
      candidate.initial_admission_epoch !== 0 ||
      (candidate.files !== undefined &&
        (!Array.isArray(candidate.files) ||
          !candidate.files.every((file) => typeof file === 'string'))) ||
      !isUsableIdempotencyKey(candidate.idempotency_key)
    ) {
      storage.removeItem(storageKey);
      return null;
    }

    return {
      conversation_id: parseConversationId(candidate.conversation_id),
      initial_admission_epoch: 0,
      input: candidate.input,
      files: candidate.files === undefined ? [] : [...candidate.files],
      idempotency_key: candidate.idempotency_key,
    };
  } catch {
    storage.removeItem(storageKey);
    return null;
  }
};

type InitialMessageAuthorityDeps = {
  getConversation: (conversationId: ConversationId) => Promise<TChatConversation | null>;
  getTranscriptSummary: (
    conversationId: ConversationId
  ) => Promise<{ items: unknown[]; total: number }>;
};

const defaultAuthorityDeps: InitialMessageAuthorityDeps = {
  getConversation: async (conversationId) => {
    try {
      return await ipcBridge.conversation.get.invoke({ conversation_id: conversationId });
    } catch {
      return null;
    }
  },
  getTranscriptSummary: async (conversationId) =>
    ipcBridge.database.getConversationMessages.invoke({
      conversation_id: conversationId,
      page: 0,
      page_size: 1,
      content_mode: 'compact',
    }),
};

export const quarantineInitialMessageDelivery = (
  storage: InitialMessageStorage,
  storageKey: string,
  idempotencyKey: string
): void => {
  const current = readInitialMessageDelivery(storage, storageKey);
  if (current?.idempotency_key === idempotencyKey) {
    storage.removeItem(storageKey);
  }
};

/**
 * Recover a Guid/QuickStart handoff only while durable backend state still
 * proves that this is an untouched, newly-created Conversation.
 *
 * Status, transcript, or transport uncertainty is terminal for automatic
 * delivery. The record is cleared so returning to an old/Finished
 * Conversation can never manufacture another turn.
 */
export const readAuthorizedInitialMessageDelivery = async (
  storage: InitialMessageStorage,
  storageKey: string,
  conversationId: ConversationId,
  deps: InitialMessageAuthorityDeps = defaultAuthorityDeps
): Promise<PersistedInitialMessage | null> => {
  const delivery = readInitialMessageDelivery(storage, storageKey);
  if (!delivery) return null;
  if (delivery.conversation_id !== conversationId) {
    quarantineInitialMessageDelivery(
      storage,
      storageKey,
      delivery.idempotency_key
    );
    return null;
  }

  try {
    const conversation = await deps.getConversation(conversationId);
    if (
      !conversation ||
      conversation.id !== conversationId ||
      conversation.status !== 'pending' ||
      conversation.runtime?.active_turn_id != null ||
      conversation.runtime?.is_processing === true
    ) {
      quarantineInitialMessageDelivery(
        storage,
        storageKey,
        delivery.idempotency_key
      );
      return null;
    }

    const transcript = await deps.getTranscriptSummary(conversationId);
    if (
      !Array.isArray(transcript.items) ||
      transcript.items.length !== 0 ||
      !Number.isSafeInteger(transcript.total) ||
      transcript.total !== 0
    ) {
      quarantineInitialMessageDelivery(
        storage,
        storageKey,
        delivery.idempotency_key
      );
      return null;
    }

    // An authority read can overlap a new explicit handoff. Never return or
    // remove a replacement operation that this check did not inspect.
    const current = readInitialMessageDelivery(storage, storageKey);
    return current?.idempotency_key === delivery.idempotency_key
      ? current
      : null;
  } catch {
    quarantineInitialMessageDelivery(
      storage,
      storageKey,
      delivery.idempotency_key
    );
    return null;
  }
};

/**
 * Persist a retryable automatic delivery before its POST starts.
 *
 * An unresolved record wins over a repeated trigger so remounts and duplicate
 * local events cannot replace the operation identity while its outcome is
 * unknown.
 */
export const persistInitialMessageDelivery = (
  storage: InitialMessageStorage,
  storageKey: string,
  conversationId: ConversationId,
  input: string,
  files: string[]
): PersistedInitialMessage => {
  const pending = readInitialMessageDelivery(storage, storageKey);
  if (pending?.conversation_id === conversationId) return pending;
  if (pending) storage.removeItem(storageKey);

  const delivery: PersistedInitialMessage = {
    conversation_id: conversationId,
    initial_admission_epoch: 0,
    input,
    files: [...files],
    idempotency_key: uuidv7(),
  };
  storage.setItem(storageKey, JSON.stringify(delivery));
  return delivery;
};

/** Prevent StrictMode/remount overlap while still allowing a later retry. */
export const claimInitialMessageDelivery = (storageKey: string): boolean => {
  if (deliveriesInFlight.has(storageKey)) return false;
  deliveriesInFlight.add(storageKey);
  return true;
};

export const releaseInitialMessageDelivery = (storageKey: string): void => {
  deliveriesInFlight.delete(storageKey);
};

/**
 * A 409 from the initial-only endpoint is terminal proof that this automatic
 * handoff no longer owns the creation generation. Quarantine that exact key;
 * transport failures retain it for a same-key retry.
 */
export const handleInitialMessageDeliveryFailure = (
  storage: InitialMessageStorage,
  storageKey: string,
  attemptedIdempotencyKey: string | null,
  error: unknown
): void => {
  if (
    attemptedIdempotencyKey &&
    isBackendHttpError(error) &&
    error.status === 409 &&
    error.code === 'CONFLICT'
  ) {
    quarantineInitialMessageDelivery(
      storage,
      storageKey,
      attemptedIdempotencyKey
    );
    releaseInitialMessageDelivery(storageKey);
    return;
  }
  releaseInitialMessageDelivery(storageKey);
};

/** Consume only after the request promise resolves with an accepted response. */
export const completeInitialMessageDelivery = (
  storage: InitialMessageStorage,
  storageKey: string,
  acceptedIdempotencyKey: string
): boolean => {
  let consumed = false;
  try {
    const current = storage.getItem(storageKey);
    const parsed = current ? (JSON.parse(current) as unknown) : null;
    if (
      parsed &&
      typeof parsed === 'object' &&
      (parsed as Record<string, unknown>).idempotency_key === acceptedIdempotencyKey
    ) {
      storage.removeItem(storageKey);
      consumed = true;
    }
  } catch {
    // A corrupt or concurrently replaced handoff is not this delivery's
    // property to consume. Leave it for explicit recovery.
  }
  releaseInitialMessageDelivery(storageKey);
  return consumed;
};
