/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Wire-contract types for `/api/providers/*`.
 *
 * Direct mirror of the Rust types in
 * `crates/nomifun-api-types/src/provider.rs`. Keep in sync with the
 * backend spec.
 */

import type { IProvider, ModelCapability, ModelProfile, ModelTask, ModelTrait } from '@/common/config/storage';
import { parseProviderId, type ProviderId } from '@/common/types/ids';

/**
 * Provider shape returned by the backend.
 *
 * The wire uses `provider_id`; renderer code deliberately keeps using
 * `IProvider.id`. Do not collapse the two shapes or read a wire-level `id`.
 */
export interface ProviderResponse {
  provider_id: string;
  platform: string;
  name: string;
  base_url: string;
  api_key: string;
  models: string[];
  enabled: boolean;
  capabilities: ModelCapability[];
  model_context_limits?: Record<string, number>;
  model_protocols?: Record<string, string>;
  model_descriptions?: Record<string, string>;
  model_enabled?: Record<string, boolean>;
  model_health?: IProvider['model_health'];
  bedrock_config?: IProvider['bedrock_config'];
  is_full_url: boolean;
  sort_order: number;
  created_at: number;
  updated_at: number;
}

export interface CreateProviderRequest {
  /**
   * Optional caller-supplied business ID. When omitted, the server generates one.
   * When supplied, it must be a canonical lowercase UUIDv7 business ID.
   */
  provider_id?: ProviderId;
  platform: string;
  name: string;
  base_url: string;
  api_key: string;
  models?: string[];
  enabled?: boolean;
  sort_order?: number;
  capabilities?: ModelCapability[];
  model_context_limits?: Record<string, number>;
  model_protocols?: Record<string, string>;
  model_descriptions?: Record<string, string>;
  model_enabled?: Record<string, boolean>;
  model_health?: IProvider['model_health'];
  bedrock_config?: IProvider['bedrock_config'];
  is_full_url?: boolean;
}

/**
 * Renderer-facing create input. The internal provider model keeps `id`; the
 * request mapper below is the only place that renames it to `provider_id`.
 */
export type CreateProviderInput = Omit<CreateProviderRequest, 'provider_id'> & {
  id?: ProviderId;
};

/** Strictly convert the provider wire response into the renderer model. */
export function fromProviderResponse(response: ProviderResponse): IProvider {
  return {
    id: parseProviderId(response.provider_id),
    platform: response.platform,
    name: response.name,
    base_url: response.base_url,
    api_key: response.api_key,
    models: response.models,
    enabled: response.enabled,
    capabilities: response.capabilities,
    model_context_limits: response.model_context_limits,
    model_protocols: response.model_protocols,
    model_descriptions: response.model_descriptions,
    model_enabled: response.model_enabled,
    model_health: response.model_health,
    bedrock_config: response.bedrock_config,
    is_full_url: response.is_full_url,
    sort_order: response.sort_order,
  };
}

/** Convert the renderer create shape into the exact backend request shape. */
export function toCreateProviderRequest(input: CreateProviderInput): CreateProviderRequest {
  return {
    ...(input.id === undefined ? {} : { provider_id: parseProviderId(input.id) }),
    platform: input.platform,
    name: input.name,
    base_url: input.base_url,
    api_key: input.api_key,
    models: input.models,
    enabled: input.enabled,
    sort_order: input.sort_order,
    capabilities: input.capabilities,
    model_context_limits: input.model_context_limits,
    model_protocols: input.model_protocols,
    model_descriptions: input.model_descriptions,
    model_enabled: input.model_enabled,
    model_health: input.model_health,
    bedrock_config: input.bedrock_config,
    is_full_url: input.is_full_url,
  };
}

/**
 * Partial-update shape for `PUT /api/providers/:id`.
 * Every field is optional — only fields sent are updated.
 */
export interface UpdateProviderRequest {
  platform?: string;
  name?: string;
  base_url?: string;
  api_key?: string;
  models?: string[];
  enabled?: boolean;
  sort_order?: number;
  capabilities?: ModelCapability[];
  model_context_limits?: Record<string, number>;
  model_protocols?: Record<string, string>;
  model_descriptions?: Record<string, string>;
  model_enabled?: Record<string, boolean>;
  model_health?: IProvider['model_health'];
  bedrock_config?: IProvider['bedrock_config'];
  is_full_url?: boolean;
}

/**
 * Response for `POST /api/providers/:id/models` and
 * `POST /api/providers/fetch-models`.
 */
export interface FetchModelsResponse {
  /** Mixed-shape array: bare id strings or `{ id, name }` pairs. */
  models: Array<string | { id: string; name: string }>;
  /** Present when backend auto-corrected the provider's base_url. */
  fixed_base_url?: string;
}

/**
 * Anonymous fetch-models request used by the pre-create form flow.
 * No provider row needs to exist yet — credentials travel in the body.
 */
export interface FetchModelsAnonymousRequest {
  platform: string;
  base_url?: string;
  api_key: string;
  bedrock_config?: IProvider['bedrock_config'];
  try_fix?: boolean;
}

export type ProviderHealthCheckErrorKind =
  | 'timeout'
  | 'invalid_authorization_header'
  | 'unauthorized'
  | 'forbidden'
  | 'not_found'
  | 'insufficient_quota'
  | 'aws_credentials'
  | 'invalid_request'
  | 'rate_limited'
  | 'connection_error'
  | 'api_error'
  | 'unknown';

export interface ProviderHealthCheckRequest {
  provider_id: ProviderId;
  model: string;
  /**
   * Which task to probe. Omit → backend uses the model's stored profile primary
   * task, falling back to a name/platform heuristic. Send an explicit task so
   * image/tts/asr models are probed at the correct endpoint.
   */
  task?: ModelTask;
}

export interface ProviderHealthCheckResponse {
  provider_id: ProviderId;
  platform: string;
  model: string;
  status: 'unknown' | 'healthy' | 'unhealthy';
  elapsed_ms: number;
  message?: string;
  error_kind?: ProviderHealthCheckErrorKind;
  http_status?: number;
  timeout_stage?: string;
}

// ---------------------------------------------------------------------------
// Model-profile endpoints (multimodal model hub) — mirror
// crates/backend/nomifun-api-types/src/{model_task,model_catalog}.rs
// ---------------------------------------------------------------------------

/** Body for `POST /api/model-profiles` (upsert one profile). */
export interface ModelProfileUpsertRequest {
  provider_id: ProviderId;
  model: string;
  tasks: ModelTask[];
  traits: ModelTrait[];
  params?: Record<string, unknown>;
  /** Defaults to 'user' server-side (this is the user-edit path). */
  source?: ModelProfile['source'];
}

/** Body identifying a single profile (`POST /api/model-profiles/delete`). */
export interface ModelProfileKeyRequest {
  provider_id: ProviderId;
  model: string;
}

/** A concrete (provider, model) selection returned by resolve. */
export interface CatalogModelRef {
  provider_id: ProviderId;
  model: string;
}

/** Body for `POST /api/model-profiles/resolve`. */
export interface ResolveModelsRequest {
  task: ModelTask;
  required_traits?: ModelTrait[];
}

export interface ResolveModelsResponse {
  models: CatalogModelRef[];
}
