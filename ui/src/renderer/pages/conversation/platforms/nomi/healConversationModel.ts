/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IProvider, TProviderWithModel } from '@/common/config/storage';

type SavedDefault = { id: string; use_model: string } | undefined;

/**
 * If `bound` points at a provider/model no longer available, resolve a
 * replacement (saved default → first available). Returns null when no heal
 * is needed or nothing is available.
 */
export function resolveHealModel(
  bound: TProviderWithModel | undefined,
  providers: IProvider[],
  getAvailableModels: (p: IProvider) => string[],
  savedDefault: SavedDefault
): { provider: IProvider; use_model: string } | null {
  if (!providers.length) return null;

  const boundProvider = bound?.id ? providers.find((p) => p.id === bound.id) : undefined;
  const boundStillValid =
    !!boundProvider && !!bound?.use_model && getAvailableModels(boundProvider).includes(bound.use_model);
  if (boundStillValid) return null;
  // 如果会话本就没绑定任何模型（空 id），交给已有 noModelSelected 流程，不在此自愈
  if (!bound?.id) return null;

  if (savedDefault) {
    const dp = providers.find((p) => p.id === savedDefault.id);
    if (dp && getAvailableModels(dp).includes(savedDefault.use_model)) {
      return { provider: dp, use_model: savedDefault.use_model };
    }
  }
  const first = providers[0];
  const firstModel = getAvailableModels(first)[0];
  if (!firstModel) return null;
  return { provider: first, use_model: firstModel };
}
