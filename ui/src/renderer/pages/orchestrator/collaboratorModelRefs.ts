import type { TModelRef } from '@/common/types/orchestrator/orchestratorTypes';

export interface ModelRefReconciliation {
  retained: TModelRef[];
  active: TModelRef[];
  removed: TModelRef[];
}

const MODEL_REF_SEPARATOR = '\0';

export const modelRefKey = (ref: TModelRef): string =>
  `${ref.provider_id}${MODEL_REF_SEPARATOR}${ref.model}`;

export const sameModelRefs = (left: TModelRef[], right: TModelRef[]): boolean =>
  left.length === right.length && left.every((item, index) => modelRefKey(item) === modelRefKey(right[index]));

export const reconcileModelRefs = (
  refs: TModelRef[],
  configuredPairs: TModelRef[],
  availablePairs: TModelRef[]
): ModelRefReconciliation => {
  const configured = new Set(configuredPairs.map(modelRefKey));
  const available = new Set(availablePairs.map(modelRefKey));
  const seen = new Set<string>();
  const retained: TModelRef[] = [];
  const active: TModelRef[] = [];
  const removed: TModelRef[] = [];

  for (const item of refs) {
    const key = modelRefKey(item);
    if (seen.has(key)) continue;
    seen.add(key);
    if (!configured.has(key)) {
      removed.push(item);
      continue;
    }
    retained.push(item);
    if (available.has(key)) active.push(item);
  }

  return { retained, active, removed };
};
