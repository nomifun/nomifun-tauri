/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type LocalModelCapabilityKey =
  | 'text'
  | 'image'
  | 'speech_recognition'
  | 'speech_synthesis';

export interface LocalModelCapabilityDefinition {
  key: LocalModelCapabilityKey;
  implemented: boolean;
}

export const LOCAL_MODEL_CAPABILITIES: readonly LocalModelCapabilityDefinition[] = [
  { key: 'text', implemented: true },
  { key: 'image', implemented: true },
  { key: 'speech_recognition', implemented: true },
  { key: 'speech_synthesis', implemented: false },
];

export type ModelTransferPhase =
  | 'not_installed'
  | 'downloading'
  | 'verifying'
  | 'extracting'
  | 'installed'
  | 'paused'
  | 'failed';

export type CapabilityActivity = 'idle' | 'running' | 'error';

const ACTIVE_TRANSFER_PHASES: readonly ModelTransferPhase[] = [
  'downloading',
  'verifying',
  'extracting',
];

const EXPANDED_DETAIL_PHASES: readonly ModelTransferPhase[] = [
  ...ACTIVE_TRANSFER_PHASES,
  'paused',
  'failed',
];

export const detailsForcedOpen = (phase: ModelTransferPhase, hasError: boolean): boolean =>
  hasError || EXPANDED_DETAIL_PHASES.includes(phase);

export const capabilityActivity = (
  phases: readonly ModelTransferPhase[],
  hasError: boolean
): CapabilityActivity => {
  if (hasError || phases.includes('failed')) return 'error';
  return phases.some((phase) => ACTIVE_TRANSFER_PHASES.includes(phase)) ? 'running' : 'idle';
};
