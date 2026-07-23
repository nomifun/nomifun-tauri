/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { ConfigKeyMap } from './configKeys';
import type { IProvider } from './storage';

export const IMAGE_GEN_ENV_KEYS = {
  providerId: 'NOMIFUN_IMG_PROVIDER_ID',
  platform: 'NOMIFUN_IMG_PLATFORM',
  baseUrl: 'NOMIFUN_IMG_BASE_URL',
  apiKey: 'NOMIFUN_IMG_API_KEY',
  model: 'NOMIFUN_IMG_MODEL',
} as const;

type ImageGenerationSelection = Partial<ConfigKeyMap['tools.imageGenerationModel']>;

export type ImageGenerationMcpEnvResolveSource = 'provider-id';

export type ImageGenerationMcpEnvResolveResult =
  | {
      ok: true;
      source: ImageGenerationMcpEnvResolveSource;
      provider: IProvider;
      model: string;
      env: Record<string, string>;
    }
  | {
      ok: false;
      reason:
        | 'missing-selection'
        | 'provider-not-found'
        | 'model-not-found';
      message: string;
    };

function providerHasModel(provider: IProvider, model: string): boolean {
  return Array.isArray(provider.models) && provider.models.includes(model);
}

function buildEnv(provider: IProvider, model: string): Record<string, string> {
  return {
    [IMAGE_GEN_ENV_KEYS.providerId]: provider.id,
    [IMAGE_GEN_ENV_KEYS.platform]: provider.platform,
    [IMAGE_GEN_ENV_KEYS.baseUrl]: provider.base_url,
    [IMAGE_GEN_ENV_KEYS.apiKey]: provider.api_key,
    [IMAGE_GEN_ENV_KEYS.model]: model,
  };
}

export function resolveImageGenerationMcpEnv(
  selection: ImageGenerationSelection | undefined,
  providers: IProvider[]
): ImageGenerationMcpEnvResolveResult {
  const providerId = selection?.provider_id;
  const model = selection?.model;

  if (!providerId) {
    return {
      ok: false,
      reason: 'missing-selection',
      message: 'Image generation provider ID is missing.',
    };
  }

  if (!model) {
    return {
      ok: false,
      reason: 'missing-selection',
      message: 'Image generation model selection is missing.',
    };
  }

  const provider = providers.find((item) => item.id === providerId);
  if (!provider) {
    return {
      ok: false,
      reason: 'provider-not-found',
      message: `Image generation provider was not found: ${providerId}`,
    };
  }
  if (!providerHasModel(provider, model)) {
    return {
      ok: false,
      reason: 'model-not-found',
      message: `Image generation model "${model}" was not found on provider "${provider.id}".`,
    };
  }
  return {
    ok: true,
    source: 'provider-id',
    provider,
    model,
    env: buildEnv(provider, model),
  };
}

export function removeImageGenerationEnvKeys(env: Record<string, string>): Record<string, string> {
  const next = { ...env };
  Object.values(IMAGE_GEN_ENV_KEYS).forEach((key) => {
    delete next[key];
  });
  return next;
}
