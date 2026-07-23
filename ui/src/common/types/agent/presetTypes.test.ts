import { describe, expect, test } from 'bun:test';
import type {
  CreatePresetRequest,
  Preset,
  PresetImportError,
  ResolvePresetRequest,
  SetPresetStateRequest,
} from './presetTypes';
import {
  parsePresetReference,
  parsePresetSnapshotReference,
  parsePresetTagKey,
} from './presetTypes';

const expectTypeError = (action: () => unknown) => {
  try {
    action();
  } catch (error) {
    expect(error instanceof TypeError).toBe(true);
    return;
  }
  throw new Error('Expected action to throw TypeError');
};

describe('preset references', () => {
  test('uses the explicit source discriminant instead of guessing from the value shape', () => {
    const userId = '0190f5fe-7c00-7a00-8000-000000000001';
    expect(parsePresetReference(userId, 'user')).toBe(userId);
    expect(parsePresetReference(userId, 'builtin')).toBe(userId);
  });

  test('rejects catalog keys, prefix_UUIDv7, and noncanonical IDs for every source', () => {
    const userId = '0190f5fe-7c00-7a00-8000-000000000001';
    expectTypeError(() => parsePresetReference(`preset_${userId}`, 'user'));
    for (const source of ['builtin', 'extension', 'user'] as const) {
      expectTypeError(() => parsePresetReference('office', source));
      expectTypeError(() => parsePresetReference(`vendor:preset_${userId}`, source));
      expectTypeError(() => parsePresetReference(' office ', source));
    }
  });

  test('validates source-less snapshot references as UUIDv7 only', () => {
    const userId = '0190f5fe-7c00-7a00-8000-000000000001';
    expect(parsePresetSnapshotReference(userId)).toBe(userId);
    expectTypeError(() => parsePresetSnapshotReference('office'));
    expectTypeError(() => parsePresetSnapshotReference(`preset_${userId}`));
  });

  test('uses preset_id across product preset response and request contracts', () => {
    const compileTimeContract = (_value: {
      preset: Pick<Preset, 'preset_id' | 'source_key'>;
      create: Pick<CreatePresetRequest, 'preset_id'>;
      state: Pick<SetPresetStateRequest, 'preset_id'>;
      resolve: Pick<ResolvePresetRequest, 'preset_id'>;
      importError: Pick<PresetImportError, 'preset_id'>;
    }) => undefined;

    compileTimeContract({
      preset: { preset_id: parsePresetReference('0190f5fe-7c00-7a00-8000-000000000002', 'builtin'), source_key: 'builtin:office' },
      create: {},
      state: { preset_id: parsePresetReference('0190f5fe-7c00-7a00-8000-000000000002', 'builtin') },
      resolve: { preset_id: parsePresetReference('0190f5fe-7c00-7a00-8000-000000000002', 'builtin') },
      importError: { preset_id: '0190f5fe-7c00-7a00-8000-000000000002' },
    });
  });
});

describe('preset tag natural keys', () => {
  test('accepts the unified builtin/user natural-key grammar', () => {
    for (const key of ['office', 'research-2', 'extension.tag', 'vendor:tag', 'tag_name']) {
      expect(parsePresetTagKey(key)).toBe(key);
    }
  });

  test('rejects UUID/natural-key dual-track and noncanonical values', () => {
    for (const key of [
      '',
      ' Research',
      'Research',
      'research/tag',
      1,
    ]) {
      expectTypeError(() => parsePresetTagKey(key));
    }
  });
});
