/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type UpdateErrorMessageKey =
  | 'update.releaseFeedUnavailable'
  | 'update.crossDeviceInstallUnsupported'
  | 'update.checkFailed';

export function getUpdateErrorMessageKey(message: unknown): UpdateErrorMessageKey {
  const normalized = String(message ?? '').toLowerCase();
  if (
    normalized.includes('nomifun_updater_auto_install_unsupported') ||
    normalized.includes('cross-device link') ||
    normalized.includes('crosses devices') ||
    normalized.includes('os error 18')
  ) {
    return 'update.crossDeviceInstallUnsupported';
  }
  if (
    normalized.includes('valid release json') ||
    normalized.includes('release json') ||
    normalized.includes('latest.json')
  ) {
    return 'update.releaseFeedUnavailable';
  }
  return 'update.checkFailed';
}
