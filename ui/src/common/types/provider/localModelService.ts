/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ModelTask, ModelTrait } from '@/common/config/storage';

/** Immutable metadata from NomiFun's curated, built-in local-model catalog. */
export interface LocalModelCatalogEntry {
  id: string;
  name: string;
  description: string;
  parameterSize: string;
  quantization: string;
  downloadSizeBytes: number;
  requiredMemoryBytes: number;
  contextWindow: number;
  license: string;
  source: string;
  recommended: boolean;
  tasks: ModelTask[];
  traits: ModelTrait[];
}

export type LocalModelInstallPhase =
  | 'not_installed'
  | 'downloading'
  | 'verifying'
  | 'installed'
  | 'paused'
  | 'failed';

export type LocalModelRuntimePhase = 'stopped' | 'starting' | 'ready' | 'stopping' | 'failed';

export type LocalModelProgressComponent =
  | 'runtime'
  | 'model'
  | 'asr_auxiliary'
  | 'vision_projector';

export type LocalModelErrorKind =
  | 'network'
  | 'insufficient_space'
  | 'checksum_mismatch'
  | 'unsupported_platform'
  | 'runtime_unavailable'
  | 'busy'
  | 'not_found'
  | 'unknown';

export interface LocalModelTransferProgress {
  component: LocalModelProgressComponent;
  downloadedBytes: number;
  totalBytes: number;
  bytesPerSecond: number;
}

/** Mutable installation/runtime state, keyed to a catalog entry by modelId. */
export interface LocalModelState {
  modelId: string;
  installPhase: LocalModelInstallPhase;
  progress: LocalModelTransferProgress | null;
  installedBytes: number;
  runtimePhase: LocalModelRuntimePhase;
  errorKind: LocalModelErrorKind | null;
  /** Backend-sanitized, user-safe detail. Never include a path, URL, or command output. */
  message: string | null;
}

export type LocalModelRuntimeBackend = 'cpu' | 'vulkan' | 'metal';

export interface LocalRuntimeStatus {
  version: string | null;
  backend: LocalModelRuntimeBackend | null;
  phase: LocalModelRuntimePhase;
  errorKind: LocalModelErrorKind | null;
  /** Backend-sanitized, user-safe detail. Never include a path, URL, or command output. */
  message: string | null;
}

export interface LocalModelServiceStatus {
  kind: 'local';
  protocolVersion: string;
  providerId: 'nomifun-local-model';
  enabled: boolean;
  ready: boolean;
  activeModelId: string | null;
  runtime: LocalRuntimeStatus;
  models: LocalModelState[];
  lastError: string | null;
}

export interface LocalModelIdRequest {
  id: string;
}

export interface SetLocalModelActiveRequest extends LocalModelIdRequest {
  enabled: boolean;
}
