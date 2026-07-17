/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Wire-contract types for NomiFun-managed model services.
 *
 * Managed services expose an OpenAI-compatible loopback endpoint to the rest
 * of the platform while keeping upstream-specific details behind a stable
 * provider entity.
*/

import type { ProviderId } from '@/common/types/ids';

export type ManagedModelServiceAvailability = 'unverified' | 'ready' | 'degraded';

export interface ManagedModel {
  id: string;
  name: string;
  enabled: boolean;
  /** Backend source key. The UI must map this to a non-sensitive display alias. */
  source: string;
}

export type ManagedModelHealthStatus = 'unknown' | 'healthy' | 'unhealthy';

export type ManagedModelHealthErrorKind =
  | 'service_disabled'
  | 'model_disabled'
  | 'busy'
  | 'timeout'
  | 'unavailable'
  | 'invalid_response'
  | 'unknown';

/** One real inference probe through the managed free-model endpoint. */
export interface ManagedModelHealthResult {
  modelId: string;
  status: ManagedModelHealthStatus;
  latencyMs: number | null;
  checkedAt: number;
  errorKind: ManagedModelHealthErrorKind | null;
  message: string | null;
}

/** Aggregate returned after checking the complete managed free-model list. */
export interface ManagedModelHealthBatchResult {
  results: ManagedModelHealthResult[];
  total: number;
  healthy: number;
  unhealthy: number;
  unknown: number;
}

export interface ManagedModelServiceStatus {
  protocolVersion: string;
  providerId: ProviderId | null;
  enabled: boolean;
  ready: boolean;
  upstream: string;
  models: ManagedModel[];
  /** Unix epoch milliseconds for the most recent successful live refresh. */
  lastRefresh: number | null;
  lastError: string | null;
  /** Whether the catalog is refreshed periodically in the background. */
  automaticRefresh: boolean;
  /** Nominal background refresh cadence in milliseconds. */
  refreshIntervalMs: number;
  /** Next scheduled attempt as epoch milliseconds. */
  nextRefresh: number | null;
  privacyNotice: string;
  availability: ManagedModelServiceAvailability;
}

export interface SetManagedModelServiceEnabledRequest {
  enabled: boolean;
}

export interface SetManagedModelEnabledRequest {
  id: string;
  enabled: boolean;
}

export interface CheckManagedModelHealthRequest {
  id: string;
}

export const NOMIFUN_FREE_MODEL_PLATFORM = 'nomifun-free-model';

/** Managed providers have dedicated UIs and must not be edited by generic CRUD. */
export const isManagedModelProvider = (provider: { id?: string; platform?: string }): boolean =>
  provider.platform === NOMIFUN_FREE_MODEL_PLATFORM;
