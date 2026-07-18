import { describe, expect, test } from 'bun:test';
import { parseConversationId } from '@/common/types/ids';
import {
  advanceConversationStopAttemptGuard,
  createConversationStopAttemptGuardState,
  getConversationStopAttemptStatus,
  isConversationStopAttemptCurrent,
  shouldReleaseStopInteraction,
  unmountConversationStopAttemptGuard,
} from './useConversationStopAttemptGuard';

const firstConversation = parseConversationId('conv_0190f5fe-7c00-7a00-8000-000000000031');
const secondConversation = parseConversationId('conv_0190f5fe-7c00-7a00-8000-000000000032');

describe('conversation stop attempt guard', () => {
  test('invalidates older attempts when a newer stop starts', () => {
    let state = advanceConversationStopAttemptGuard(
      createConversationStopAttemptGuardState(firstConversation),
      { mounted: true }
    );
    state = advanceConversationStopAttemptGuard(state);
    const firstToken = {
      conversationId: state.conversationId,
      generation: state.generation,
      turnStartGeneration: 4,
      turnCompletionGeneration: 2,
    };
    expect(isConversationStopAttemptCurrent(state, firstToken, 4, 2)).toBe(true);

    state = advanceConversationStopAttemptGuard(state);
    expect(isConversationStopAttemptCurrent(state, firstToken, 4, 2)).toBe(false);
    expect(getConversationStopAttemptStatus(state, firstToken, 4, 2)).toBe('superseded');
  });

  test('invalidates a stop result when a newer local or authoritative turn starts', () => {
    let state = advanceConversationStopAttemptGuard(
      createConversationStopAttemptGuardState(firstConversation),
      { mounted: true }
    );
    state = advanceConversationStopAttemptGuard(state);
    const token = {
      conversationId: state.conversationId,
      generation: state.generation,
      turnStartGeneration: 7,
      turnCompletionGeneration: 3,
    };

    expect(getConversationStopAttemptStatus(state, token, 7, 3)).toBe('current');
    expect(getConversationStopAttemptStatus(state, token, 8, 3)).toBe('turn_started');
    expect(isConversationStopAttemptCurrent(state, token, 8, 3)).toBe(false);
  });

  test('invalidates failure restoration when authoritative completion wins the race', () => {
    let state = advanceConversationStopAttemptGuard(
      createConversationStopAttemptGuardState(firstConversation),
      { mounted: true }
    );
    state = advanceConversationStopAttemptGuard(state);
    const token = {
      conversationId: state.conversationId,
      generation: state.generation,
      turnStartGeneration: 7,
      turnCompletionGeneration: 3,
    };

    expect(getConversationStopAttemptStatus(state, token, 7, 4)).toBe('turn_completed');
    expect(isConversationStopAttemptCurrent(state, token, 7, 4)).toBe(false);
    expect(shouldReleaseStopInteraction('turn_completed')).toBe(true);
    expect(shouldReleaseStopInteraction('turn_started')).toBe(true);
    expect(shouldReleaseStopInteraction('superseded')).toBe(false);
  });

  test('invalidates an in-flight promise on conversation switch and unmount', () => {
    let state = advanceConversationStopAttemptGuard(
      createConversationStopAttemptGuardState(firstConversation),
      { mounted: true }
    );
    state = advanceConversationStopAttemptGuard(state);
    const token = {
      conversationId: state.conversationId,
      generation: state.generation,
      turnStartGeneration: 1,
      turnCompletionGeneration: 0,
    };

    state = advanceConversationStopAttemptGuard(state, { conversationId: secondConversation });
    expect(isConversationStopAttemptCurrent(state, token, 1, 0)).toBe(false);

    state = advanceConversationStopAttemptGuard(state);
    const secondToken = {
      conversationId: state.conversationId,
      generation: state.generation,
      turnStartGeneration: 1,
      turnCompletionGeneration: 0,
    };
    state = unmountConversationStopAttemptGuard(state, firstConversation);
    expect(isConversationStopAttemptCurrent(state, secondToken, 1, 0)).toBe(true);

    state = unmountConversationStopAttemptGuard(state, secondConversation);
    expect(isConversationStopAttemptCurrent(state, secondToken, 1, 0)).toBe(false);
  });
});
