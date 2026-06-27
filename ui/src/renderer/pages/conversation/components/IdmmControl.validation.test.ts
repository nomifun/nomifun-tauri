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

const modelWatch = (overrides: Partial<IdmmWatchBackupConfig> = {}): IdmmWatchBackupConfig => ({
  enabled: true,
  tier: 'rule_plus_model',
  bypass_model: { provider_id: null, model: null },
  ...overrides,
});

describe('IDMM backup model validation', () => {
  test('allows a model-tier watch to use the global backup model when local fields are empty', () => {
    const watch = modelWatch();

    expect(isWatchBackupReady(watch, true)).toBe(true);
    expect(getWatchBackupValidationErrorKey(watch, true)).toBeNull();
  });

  test('blocks enabling when a local backup provider is selected without a model', () => {
    const watch = modelWatch({ bypass_model: { provider_id: 'openai', model: null } });

    expect(isWatchBackupReady(watch, true)).toBe(false);
    expect(getWatchBackupValidationErrorKey(watch, true)).toBe('idmm.backupModelIncomplete');
  });

  test('blocks enabling when no local or global backup model is available', () => {
    const watch = modelWatch();

    expect(isWatchBackupReady(watch, false)).toBe(false);
    expect(getWatchBackupValidationErrorKey(watch, false)).toBe('idmm.backupRequired');
  });

  test('does not require a backup model for disabled or rule-only watches', () => {
    expect(isWatchBackupReady(modelWatch({ enabled: false }), false)).toBe(true);
    expect(isWatchBackupReady(modelWatch({ tier: 'rule_only' }), false)).toBe(true);
  });
});
