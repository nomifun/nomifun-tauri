import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseConversationId } from '@/common/types/ids';
import {
  ConversationStopTimeoutError,
  ConversationStopConfirmationTimeoutError,
  requestConversationStop,
  stopConversationAndConfirmRelease,
  waitForConversationTurnRelease,
} from './requestConversationStop';

const conversationId = parseConversationId('conv_0190f5fe-7c00-7a00-8000-000000000021');
const runtime = (isProcessing: boolean) =>
  ({ runtime: { is_processing: isProcessing } }) as TChatConversation;

describe('conversation stop safeguards', () => {
  test('bounds a transport that never settles', async () => {
    let thrown: unknown;
    try {
      await requestConversationStop(conversationId, 5, () => new Promise<never>(() => {}));
    } catch (error) {
      thrown = error;
    }
    expect(thrown instanceof ConversationStopTimeoutError).toBe(true);
  });

  test('distinguishes a deleted conversation from an idle runtime', async () => {
    expect(await waitForConversationTurnRelease(conversationId, async () => null, [0])).toBe('deleted');
    expect(await waitForConversationTurnRelease(conversationId, async () => runtime(false), [0])).toBe('released');
  });

  test('keeps processing closed until a later authoritative read is idle', async () => {
    let reads = 0;
    const result = await waitForConversationTurnRelease(
      conversationId,
      async () => runtime(++reads < 3),
      [0, 0, 0]
    );
    expect(result).toBe('released');
    expect(reads).toBe(3);
  });

  test('does not collapse a runtime query failure into the deleted case', async () => {
    const error = new Error('network unavailable');
    let thrown: unknown;
    try {
      await waitForConversationTurnRelease(conversationId, async () => Promise.reject(error), [0]);
    } catch (caught) {
      thrown = caught;
    }
    expect(thrown).toBe(error);
  });

  test('treats a failed stop request as successful when runtime is already idle', async () => {
    const requestError = new Error('request timed out');
    const result = await stopConversationAndConfirmRelease(conversationId, {
      requestStop: async () => Promise.reject(requestError),
      waitForRelease: async () => 'released',
    });
    expect(result).toEqual({ status: 'released', requestError });
  });

  test('preserves deleted and unknown outcomes during coordinated stop confirmation', async () => {
    expect(
      await stopConversationAndConfirmRelease(conversationId, {
        requestStop: async () => undefined,
        waitForRelease: async () => 'deleted',
      })
    ).toEqual({ status: 'deleted' });

    const error = new Error('GET failed');
    expect(
      await stopConversationAndConfirmRelease(conversationId, {
        requestStop: async () => undefined,
        waitForRelease: async () => Promise.reject(error),
      })
    ).toEqual({ status: 'unknown', error });
  });

  test('bounds a confirmation read that never settles so stop interaction can be retried', async () => {
    const result = await stopConversationAndConfirmRelease(conversationId, {
      requestStop: async () => undefined,
      waitForRelease: () => new Promise<never>(() => {}),
      confirmationTimeoutMs: 5,
    });

    expect(result.status).toBe('unknown');
    expect(result.status === 'unknown' && result.error instanceof ConversationStopConfirmationTimeoutError).toBe(true);
  });
});
