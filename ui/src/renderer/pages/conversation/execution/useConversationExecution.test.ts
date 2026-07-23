import { describe, expect, test } from 'bun:test';
import { parseExecutionId } from '@/common/types/ids';
import { shouldDiscoverExecutionRelation } from './useConversationExecution';

const OLD_EXECUTION = parseExecutionId('0190f5fe-7c00-7a00-8000-000000000001');
const NEW_EXECUTION = parseExecutionId('0190f5fe-7c00-7a00-8000-000000000002');

describe('ConversationExecutionLink discovery', () => {
  test('discovers the first execution created while the conversation is already open', () => {
    expect(
      shouldDiscoverExecutionRelation(null, {
        execution_id: NEW_EXECUTION,
        change_kind: 'created',
      }),
    ).toBe(true);
  });

  test('discovers a replacement execution but leaves current execution updates to the detail subscription', () => {
    expect(
      shouldDiscoverExecutionRelation(OLD_EXECUTION, {
        execution_id: NEW_EXECUTION,
        change_kind: 'plan_changed',
      }),
    ).toBe(true);
    expect(
      shouldDiscoverExecutionRelation(OLD_EXECUTION, {
        execution_id: OLD_EXECUTION,
        change_kind: 'step_changed',
      }),
    ).toBe(false);
  });

  test('re-resolves the relation after the current execution is deleted', () => {
    expect(
      shouldDiscoverExecutionRelation(OLD_EXECUTION, {
        execution_id: OLD_EXECUTION,
        change_kind: 'deleted',
      }),
    ).toBe(true);
  });
});
