/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import type { IModelFailoverCandidate, IModelFailoverConfig } from '@/common/adapter/ipcBridge';

interface ModelFailoverSaveResult {
  config: IModelFailoverConfig;
  appendedDraft: boolean;
  hasCompleteDraft: boolean;
}

export function buildModelFailoverConfigForSave(
  config: IModelFailoverConfig,
  draftProvider?: string,
  draftModel?: string
): ModelFailoverSaveResult {
  const queue = config.queue ?? [];
  const hasCompleteDraft = Boolean(draftProvider && draftModel);
  if (!draftProvider || !draftModel) {
    return { config: { ...config, queue }, appendedDraft: false, hasCompleteDraft };
  }

  const draft: IModelFailoverCandidate = { provider_id: draftProvider, model: draftModel };
  const duplicated = queue.some((candidate) => candidate.provider_id === draft.provider_id && candidate.model === draft.model);
  if (duplicated) {
    return { config: { ...config, queue }, appendedDraft: false, hasCompleteDraft };
  }

  return { config: { ...config, queue: [...queue, draft] }, appendedDraft: true, hasCompleteDraft };
}
