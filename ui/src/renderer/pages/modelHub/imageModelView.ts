/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type {
  ImageModelComponent,
  ImageModelComponentProgress,
  ImageModelInstallPhase,
  ImageModelServiceStatus,
  ImageModelState,
} from '@/common/types/provider/imageModelService';

export type ImageModelPrimaryAction = 'install' | 'pause' | 'resume' | 'retry' | 'none';

export const IMAGE_MODEL_COMPONENTS: readonly ImageModelComponent[] = [
  'runtime',
  'diffusion_model',
  'text_encoder',
  'vae',
];

const ACTIVE_PHASES = new Set<ImageModelInstallPhase>(['downloading', 'verifying', 'extracting']);

export const emptyImageModelState = (modelId: string): ImageModelState => ({
  modelId,
  installPhase: 'not_installed',
  componentProgress: IMAGE_MODEL_COMPONENTS.map((component) => ({
    component,
    installPhase: 'not_installed',
    downloadedBytes: 0,
    totalBytes: 0,
    installedBytes: 0,
    bytesPerSecond: 0,
    errorKind: null,
    message: null,
  })),
  installedBytes: 0,
  errorKind: null,
  message: null,
});

export const stateForImageModel = (
  states: ImageModelState[] | undefined,
  modelId: string
): ImageModelState => states?.find((state) => state.modelId === modelId) ?? emptyImageModelState(modelId);

export const componentProgressFor = (
  state: ImageModelState,
  component: ImageModelComponent
): ImageModelComponentProgress =>
  state.componentProgress.find((progress) => progress.component === component) ??
  emptyImageModelState(state.modelId).componentProgress.find((progress) => progress.component === component)!;

export const imageModelProgressPercent = (downloadedBytes: number, totalBytes: number): number | null => {
  if (!Number.isFinite(totalBytes) || totalBytes <= 0 || !Number.isFinite(downloadedBytes)) return null;
  return Math.min(100, Math.max(0, (downloadedBytes / totalBytes) * 100));
};

export const imageModelProgressTotals = (state: ImageModelState) =>
  state.componentProgress.reduce(
    (total, progress) => ({
      downloadedBytes: total.downloadedBytes + progress.downloadedBytes,
      totalBytes: total.totalBytes + progress.totalBytes,
      bytesPerSecond: total.bytesPerSecond + progress.bytesPerSecond,
    }),
    { downloadedBytes: 0, totalBytes: 0, bytesPerSecond: 0 }
  );

export const imageModelPrimaryAction = (state: ImageModelState): ImageModelPrimaryAction => {
  switch (state.installPhase) {
    case 'not_installed':
      return 'install';
    case 'downloading':
    case 'verifying':
    case 'extracting':
      return 'pause';
    case 'paused':
      return 'resume';
    case 'failed':
      return state.errorKind === 'unsupported_platform' ? 'none' : 'retry';
    case 'installed':
      return 'none';
  }
};

export const canDeleteImageModel = (state: ImageModelState): boolean => {
  if (state.installPhase === 'installed' || state.installPhase === 'paused') return true;
  return (
    state.installPhase === 'failed' &&
    state.componentProgress.some((progress) => progress.downloadedBytes > 0 || progress.installedBytes > 0)
  );
};

export const isImageModelActivityPending = (status: ImageModelServiceStatus | undefined): boolean =>
  Boolean(
    status &&
      (status.runtimePhase === 'busy' ||
        status.models.some((model) => ACTIVE_PHASES.has(model.installPhase)))
  );
