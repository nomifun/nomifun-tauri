/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import {
  getWatchBackupValidationErrorKey,
  isWatchBackupReady,
  type IdmmWatchBackupConfig,
} from './IdmmControl.validation';
import { parseProviderId } from '@/common/types/ids';

const modelWatch = (overrides: Partial<IdmmWatchBackupConfig> = {}): IdmmWatchBackupConfig => ({
  enabled: true,
  tier: 'rule_plus_model',
  bypass_model: { provider_id: null, model: null },
  ...overrides,
});

describe('IDMM backup model validation', () => {
  test('allows a model-tier watch to use the global backup when its override is empty', () => {
    const watch = modelWatch();

    expect(isWatchBackupReady(watch, true)).toBe(true);
    expect(getWatchBackupValidationErrorKey(watch, true)).toBeNull();
  });

  test('blocks enabling when a conversation backup provider is selected without a model', () => {
    const watch = modelWatch({
      bypass_model: {
        provider_id: parseProviderId('prov_0190f5fe-7c00-7a00-8000-000000000001'),
        model: null,
      },
    });

    expect(isWatchBackupReady(watch, true)).toBe(false);
    expect(getWatchBackupValidationErrorKey(watch, true)).toBe('idmm.backupModelIncomplete');
  });

  test('blocks enabling when neither an override nor a global backup is available', () => {
    const watch = modelWatch();

    expect(isWatchBackupReady(watch, false)).toBe(false);
    expect(getWatchBackupValidationErrorKey(watch, false)).toBe('idmm.backupRequired');
  });

  test('does not require a backup model for disabled or rule-only watches', () => {
    expect(isWatchBackupReady(modelWatch({ enabled: false }), false)).toBe(true);
    expect(isWatchBackupReady(modelWatch({ tier: 'rule_only' }), false)).toBe(true);
  });
});
