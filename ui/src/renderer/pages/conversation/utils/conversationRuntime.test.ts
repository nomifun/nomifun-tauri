import { describe, expect, test } from 'bun:test';
import type { TChatConversation } from '@/common/config/storage';
import { parseMessageId } from '@/common/types/ids';
import {
  getConversationRuntimeAuthority,
  isConversationProcessing,
} from './conversationRuntime';

const activeTurnId = parseMessageId('0190f5fe-7c00-7a00-8000-000000000081');

describe('conversation runtime authority', () => {
  test('Finished cannot be promoted by a stale processing bit', () => {
    const snapshot = {
      status: 'finished',
      runtime: {
        is_processing: true,
        active_turn_id: activeTurnId,
      },
    } as TChatConversation;

    expect(getConversationRuntimeAuthority(snapshot)).toBe('idle');
    expect(isConversationProcessing(snapshot)).toBe(false);
  });

  test('Running requires an exact active turn projection', () => {
    const incomplete = {
      status: 'running',
      runtime: { is_processing: true },
    } as TChatConversation;
    const exact = {
      status: 'running',
      runtime: {
        is_processing: true,
        active_turn_id: activeTurnId,
      },
    } as TChatConversation;

    expect(getConversationRuntimeAuthority(incomplete)).toBe('unknown');
    expect(isConversationProcessing(incomplete)).toBe(false);
    expect(getConversationRuntimeAuthority(exact)).toBe('processing');
    expect(isConversationProcessing(exact)).toBe(true);
  });

  test('Pending is idle only when no processing projection exists', () => {
    expect(
      getConversationRuntimeAuthority({
        status: 'pending',
        runtime: { is_processing: false },
      } as TChatConversation)
    ).toBe('idle');
    expect(
      getConversationRuntimeAuthority({
        status: 'pending',
        runtime: { is_processing: true },
      } as TChatConversation)
    ).toBe('unknown');
    expect(
      getConversationRuntimeAuthority({
        status: 'pending',
        runtime: {
          is_processing: false,
          active_turn_id: activeTurnId,
        },
      } as TChatConversation)
    ).toBe('unknown');
  });
});
