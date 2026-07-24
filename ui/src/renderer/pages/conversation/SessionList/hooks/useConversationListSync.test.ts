import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseMessageId } from '@/common/types/ids';

import {
  getExactSidebarActiveTurnId,
  isGeneratingStreamMessage,
  shouldAcceptSidebarTurnCompletion,
  shouldAcceptSidebarTurnStart,
  shouldApplySidebarStreamActivity,
} from './useConversationListSync';

const oldTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000091');
const currentTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000092');
const runningConversation = {
  status: 'running',
  runtime: {
    is_processing: true,
    active_turn_id: currentTurnId,
  },
} as TChatConversation;
const finishedConversation = {
  status: 'finished',
  runtime: {
    is_processing: true,
    active_turn_id: oldTurnId,
  },
} as TChatConversation;

describe('conversation list stream activity', () => {
  test('ordinary content raises the sidebar generating state', () => {
    expect(isGeneratingStreamMessage({ type: 'content', data: { content: 'chunk' } })).toBe(true);
  });

  test('a complete assistant projection never raises a stuck sidebar spinner', () => {
    expect(
      isGeneratingStreamMessage({
        type: 'content',
        data: { content: 'final execution report' },
        stream_complete: true,
      })
    ).toBe(false);
  });

  test('Finished remains idle even when a stale runtime projection names an old turn', () => {
    expect(getExactSidebarActiveTurnId(finishedConversation)).toBeNull();
    expect(
      shouldAcceptSidebarTurnStart({
        turnId: oldTurnId,
        eventRuntimeIsProcessing: true,
        eventActiveTurnId: oldTurnId,
        conversation: finishedConversation,
      })
    ).toBe(false);
    expect(
      shouldApplySidebarStreamActivity({
        messageTurnId: oldTurnId,
        activeTurnId: getExactSidebarActiveTurnId(finishedConversation) ?? undefined,
      })
    ).toBe(false);
  });

  test('started and stream activity require the same exact current active turn id', () => {
    expect(
      shouldAcceptSidebarTurnStart({
        turnId: oldTurnId,
        eventRuntimeIsProcessing: true,
        eventActiveTurnId: oldTurnId,
        conversation: runningConversation,
      })
    ).toBe(false);
    expect(
      shouldAcceptSidebarTurnStart({
        turnId: currentTurnId,
        eventRuntimeIsProcessing: true,
        eventActiveTurnId: currentTurnId,
        conversation: runningConversation,
      })
    ).toBe(true);
    expect(
      shouldApplySidebarStreamActivity({
        messageTurnId: oldTurnId,
        activeTurnId: currentTurnId,
      })
    ).toBe(false);
    expect(
      shouldApplySidebarStreamActivity({
        messageTurnId: currentTurnId,
        activeTurnId: currentTurnId,
      })
    ).toBe(true);
  });

  test('completion is fail-closed on a mismatched root or contradictory runtime', () => {
    expect(
      shouldAcceptSidebarTurnCompletion({
        completedTurnId: oldTurnId,
        activeTurnId: currentTurnId,
        eventRuntimeIsProcessing: false,
        conversation: { status: 'finished', runtime: { is_processing: false } } as TChatConversation,
      })
    ).toBe(false);
    expect(
      shouldAcceptSidebarTurnCompletion({
        completedTurnId: currentTurnId,
        activeTurnId: currentTurnId,
        eventRuntimeIsProcessing: false,
        eventActiveTurnId: currentTurnId,
        conversation: { status: 'finished', runtime: { is_processing: false } } as TChatConversation,
      })
    ).toBe(false);
    expect(
      shouldAcceptSidebarTurnCompletion({
        completedTurnId: currentTurnId,
        activeTurnId: currentTurnId,
        eventRuntimeIsProcessing: false,
        conversation: { status: 'finished', runtime: { is_processing: false } } as TChatConversation,
      })
    ).toBe(true);
  });
});
