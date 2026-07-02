/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IProvider } from '@/common/config/storage';

export function cloneProviderConfig(provider: IProvider, nextId: string, copyLabel: string): IProvider {
  return {
    ...provider,
    id: nextId,
    name: `${provider.name} ${copyLabel}`.trim(),
    model_health: undefined,
  };
}
