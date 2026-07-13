/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type {
  LocalModelState,
  LocalRuntimeStatus,
} from './localModelService';

export type AsrEngine = 'whisper_cpp' | 'fun_asr_llama_cpp';
export type AsrCapability =
  | 'transcription'
  | 'language_detection'
  | 'emotion_detection'
  | 'audio_event_detection'
  | 'long_audio_vad';

/** Immutable metadata from NomiFun's curated local speech-recognition catalog. */
export interface AsrModelCatalogEntry {
  id: string;
  name: string;
  description: string;
  modelSize: string;
  quantization: string;
  downloadSizeBytes: number;
  requiredMemoryBytes: number;
  languages: string[];
  license: string;
  source: string;
  recommended: boolean;
  engine: AsrEngine;
  capabilities: AsrCapability[];
}

/**
 * Local ASR runtime status. The selected engine is launched only for one
 * transcription request, so `ready` means its verified one-shot runtime and
 * active model are available rather than that a resident process is running.
 */
export interface AsrModelServiceStatus {
  protocolVersion: string;
  enabled: boolean;
  ready: boolean;
  activeModelId: string | null;
  runtime: LocalRuntimeStatus;
  models: LocalModelState[];
  lastError: string | null;
}

export interface AsrModelIdRequest {
  id: string;
}

export interface SetAsrModelActiveRequest extends AsrModelIdRequest {
  enabled: boolean;
}
