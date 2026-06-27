/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type IdmmBackupValidationKey = 'idmm.backupRequired' | 'idmm.backupModelIncomplete';

export type IdmmWatchBackupConfig = {
  enabled: boolean;
  tier: string;
  bypass_model: {
    provider_id?: string | null;
    model?: string | null;
  };
};

const hasText = (value?: string | null): boolean => Boolean(value?.trim());

export const getWatchBackupValidationErrorKey = (
  watch: IdmmWatchBackupConfig,
  globalBackupResolved: boolean
): IdmmBackupValidationKey | null => {
  if (!watch.enabled || watch.tier !== 'rule_plus_model') return null;

  const hasLocalProvider = hasText(watch.bypass_model.provider_id);
  const hasLocalModel = hasText(watch.bypass_model.model);

  if (hasLocalProvider !== hasLocalModel) return 'idmm.backupModelIncomplete';
  if (!hasLocalProvider && !globalBackupResolved) return 'idmm.backupRequired';
  return null;
};

export const isWatchBackupReady = (watch: IdmmWatchBackupConfig, globalBackupResolved: boolean): boolean =>
  getWatchBackupValidationErrorKey(watch, globalBackupResolved) === null;
