/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { LocalModelErrorKind } from './localModelService';

/** One independently downloaded and verified part of a local image bundle. */
export type ImageModelComponent = 'runtime' | 'diffusion_model' | 'text_encoder' | 'vae';

export type ImageModelInstallPhase =
  | 'not_installed'
  | 'downloading'
  | 'verifying'
  | 'extracting'
  | 'installed'
  | 'paused'
  | 'failed';

/** The image runtime is launched once per creation job instead of staying resident. */
export type ImageModelRuntimePhase = 'unavailable' | 'ready' | 'busy' | 'failed';

/** Immutable metadata from NomiFun's curated local image-model catalog. */
export interface ImageModelCatalogEntry {
  id: string;
  name: string;
  description: string;
  format: string;
  /** Runtime archive plus every model artifact for the current platform. */
  downloadSizeBytes: number;
  requiredMemoryBytes: number;
  license: string;
  source: string;
  components: ImageModelComponent[];
  recommended: boolean;
  /** Upstream provenance or licensing qualification that must remain visible. */
  notice: string | null;
}

export interface ImageModelComponentProgress {
  component: ImageModelComponent;
  installPhase: ImageModelInstallPhase;
  downloadedBytes: number;
  totalBytes: number;
  installedBytes: number;
  bytesPerSecond: number;
  errorKind: LocalModelErrorKind | null;
  /** Backend-sanitized detail; never contains paths, URLs, hashes, or process output. */
  message: string | null;
}

export interface ImageModelState {
  modelId: string;
  installPhase: ImageModelInstallPhase;
  componentProgress: ImageModelComponentProgress[];
  installedBytes: number;
  errorKind: LocalModelErrorKind | null;
  /** Backend-sanitized detail; never contains paths, URLs, hashes, or process output. */
  message: string | null;
}

export interface ImageModelServiceStatus {
  protocolVersion: string;
  /** Every managed artifact is installed and recorded by the pinned manifest. */
  artifactsReady: boolean;
  /** The installed bundle has also passed the current process's integrity check. */
  inferenceReady: boolean;
  runtimePhase: ImageModelRuntimePhase;
  models: ImageModelState[];
  lastError: string | null;
}

export interface ImageModelIdRequest {
  id: string;
}
