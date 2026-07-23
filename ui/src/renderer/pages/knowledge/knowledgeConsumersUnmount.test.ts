import { describe, expect, test } from 'bun:test';
import type { IKnowledgeBinding } from '@/common/adapter/ipcBridge';
import { removeBaseFromBinding } from './KnowledgeConsumersSection';
import { parseKnowledgeBaseId } from '@/common/types/ids';

const KB_A = parseKnowledgeBaseId('019b0000-0000-7000-8000-000000000001');
const KB_B = parseKnowledgeBaseId('019b0000-0000-7000-8000-000000000002');
const KB_MISSING = parseKnowledgeBaseId('019b0000-0000-7000-8000-000000000003');

const binding = (overrides: Partial<IKnowledgeBinding> = {}): IKnowledgeBinding => ({
  enabled: true,
  writeback: true,
  writeback_mode: 'direct',
  writeback_eagerness: 'aggressive',
  channel_write_enabled: true,
  kb_ids: [KB_A, KB_B],
  ...overrides,
});

describe('knowledge consumer unmount binding transform', () => {
  test('removes only the requested base and preserves binding policy fields', () => {
    expect(removeBaseFromBinding(binding(), KB_A)).toEqual({
      enabled: true,
      writeback: true,
      writeback_mode: 'direct',
      writeback_eagerness: 'aggressive',
      channel_write_enabled: true,
      kb_ids: [KB_B],
    });
  });

  test('turns the binding off when the last mounted base is removed', () => {
    expect(removeBaseFromBinding(binding({ kb_ids: [KB_A] }), KB_A)).toEqual({
      enabled: false,
      writeback: true,
      writeback_mode: 'direct',
      writeback_eagerness: 'aggressive',
      channel_write_enabled: true,
      kb_ids: [],
    });
  });

  test('keeps a non-empty binding enabled when the requested base is not present', () => {
    expect(removeBaseFromBinding(binding(), KB_MISSING)).toEqual(binding());
  });
});
