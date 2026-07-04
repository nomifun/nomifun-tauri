/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

export type ProviderUsageFeature = 'desktopCompanion' | 'publicCompanion' | 'smartDecision' | 'orchestrator';

export interface ProviderUsage {
  feature: ProviderUsageFeature;
  label: string;
  targetId?: string;
}

/** Deep-link route for a feature's unbind location. Verify against Router.tsx. */
export function featureRoute(feature: ProviderUsageFeature, targetId?: string): string {
  switch (feature) {
    case 'desktopCompanion':
      // Desktop-companion model control (CompanionModelControl → profile.model)
      // lives on the Nomi config page; /companion is the transparent pet overlay.
      return '/nomi';
    case 'publicCompanion':
      return targetId ? `/public-companions/${targetId}` : '/public-companions';
    case 'smartDecision':
      // IDMM global backup model lives in Global Model Config → IDMM tab,
      // where backup_provider_id / backup_model can be cleared to unbind.
      return '/models?section=global';
    case 'orchestrator':
      return '/guid';
  }
}

export interface ProviderUsageGroup {
  feature: ProviderUsageFeature;
  labels: string[];
  targetId?: string;
}

export function groupUsagesByFeature(usages: ProviderUsage[]): ProviderUsageGroup[] {
  const map = new Map<ProviderUsageFeature, ProviderUsageGroup>();
  for (const u of usages) {
    const g = map.get(u.feature) ?? { feature: u.feature, labels: [], targetId: u.targetId };
    g.labels.push(u.label);
    map.set(u.feature, g);
  }
  return [...map.values()];
}

/** Extract usages from a BackendHttpError.details payload. */
export function parseProviderInUseDetails(details: unknown): ProviderUsage[] {
  if (details && typeof details === 'object' && Array.isArray((details as { usages?: unknown }).usages)) {
    return (details as { usages: ProviderUsage[] }).usages;
  }
  return [];
}
