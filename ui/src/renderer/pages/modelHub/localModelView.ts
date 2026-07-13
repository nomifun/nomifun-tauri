/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type {
  LocalModelInstallPhase,
  LocalModelServiceStatus,
  LocalModelState,
  LocalModelTransferProgress,
} from '@/common/types/provider/localModelService';

export type LocalModelPrimaryAction = 'install' | 'cancel' | 'resume' | 'retry' | 'activate' | 'deactivate' | 'none';

const ACTIVE_INSTALL_PHASES = new Set<LocalModelInstallPhase>(['downloading', 'verifying']);

export const emptyLocalModelState = (modelId: string): LocalModelState => ({
  modelId,
  installPhase: 'not_installed',
  progress: null,
  installedBytes: 0,
  runtimePhase: 'stopped',
  errorKind: null,
  message: null,
});

export const stateForLocalModel = (
  states: LocalModelState[] | undefined,
  modelId: string
): LocalModelState => states?.find((state) => state.modelId === modelId) ?? emptyLocalModelState(modelId);

export const isLocalModelActivityPending = (
  status: Pick<LocalModelServiceStatus, 'models' | 'runtime'> | undefined
): boolean =>
  Boolean(
    status &&
      (status.models.some((model) => ACTIVE_INSTALL_PHASES.has(model.installPhase)) ||
        status.runtime.phase === 'starting' ||
        status.runtime.phase === 'stopping')
  );

export const localModelProgressPercent = (progress: LocalModelTransferProgress | null): number | null => {
  if (!progress || !Number.isFinite(progress.totalBytes) || progress.totalBytes <= 0) return null;
  if (!Number.isFinite(progress.downloadedBytes)) return null;
  return Math.min(100, Math.max(0, (progress.downloadedBytes / progress.totalBytes) * 100));
};

export const localModelPrimaryAction = (
  state: LocalModelState,
  isActive: boolean
): LocalModelPrimaryAction => {
  switch (state.installPhase) {
    case 'not_installed':
      return 'install';
    case 'downloading':
      return 'cancel';
    case 'verifying':
      return 'cancel';
    case 'paused':
      return 'resume';
    case 'failed':
      return 'retry';
    case 'installed':
      return isActive ? 'deactivate' : 'activate';
  }
};

export const canDeleteLocalModel = (state: LocalModelState, isActive: boolean): boolean =>
  !isActive && (state.installPhase === 'installed' || state.installPhase === 'paused' || state.installPhase === 'failed');

export const formatLocalModelBytes = (bytes: number, locale?: string): string => {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  const digits = value >= 100 || exponent === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits }).format(value)} ${units[exponent]}`;
};

export const formatLocalModelRate = (bytesPerSecond: number, locale?: string): string =>
  `${formatLocalModelBytes(bytesPerSecond, locale)}/s`;
