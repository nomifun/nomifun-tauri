/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { LocalModelServiceStatus } from '@/common/types/provider/localModelService';
import { MODEL_PROFILES_SWR_KEY } from '@/renderer/hooks/agent/useModelProfiles';
import { PROVIDERS_SWR_KEY } from '@/renderer/hooks/agent/useModelProviderList';
import { useCallback, useRef, useState } from 'react';
import useSWR, { mutate as mutateGlobal, type KeyedMutator } from 'swr';
import { isLocalModelActivityPending } from './localModelView';

export const LOCAL_MODEL_CATALOG_SWR_KEY = 'model-services/local/catalog';
export const LOCAL_MODEL_STATUS_SWR_KEY = 'model-services/local/status';

const fetchLocalCatalog = () => ipcBridge.managedModelService.local.catalog.invoke();

let lastProjectionSignature = '';

const fetchLocalStatus = async () => {
  const status = await ipcBridge.managedModelService.local.status.invoke();
  // Download progress does not affect provider selectors. Refresh those caches
  // only when the projected model set or active service selection changes.
  const projectionSignature = JSON.stringify({
    enabled: status.enabled,
    activeModelId: status.activeModelId,
    installedModels: status.models
      .filter((model) => model.installPhase === 'installed')
      .map((model) => model.modelId)
      .sort(),
  });
  if (lastProjectionSignature && projectionSignature !== lastProjectionSignature) {
    void mutateGlobal(PROVIDERS_SWR_KEY);
    void mutateGlobal(MODEL_PROFILES_SWR_KEY);
  }
  lastProjectionSignature = projectionSignature;
  return status;
};

/** State and serialized mutations for the one-runtime, one-active-model local service. */
export const useLocalModels = () => {
  const catalogQuery = useSWR(LOCAL_MODEL_CATALOG_SWR_KEY, fetchLocalCatalog, {
    revalidateOnFocus: false,
    revalidateOnReconnect: true,
    shouldRetryOnError: false,
  });
  const statusQuery = useSWR<LocalModelServiceStatus>(LOCAL_MODEL_STATUS_SWR_KEY, fetchLocalStatus, {
    revalidateOnFocus: false,
    revalidateOnReconnect: true,
    shouldRetryOnError: false,
    refreshInterval: (latestStatus) => (isLocalModelActivityPending(latestStatus) ? 1_000 : 10_000),
  });
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const pendingActionRef = useRef<string | null>(null);
  const mutateStatus: KeyedMutator<LocalModelServiceStatus> = statusQuery.mutate;

  const installStatus = useCallback(
    async (status: LocalModelServiceStatus) => {
      await mutateStatus(status, false);
      await Promise.all([mutateGlobal(PROVIDERS_SWR_KEY), mutateGlobal(MODEL_PROFILES_SWR_KEY)]);
      return status;
    },
    [mutateStatus]
  );

  const runAction = useCallback(
    async (key: string, action: () => Promise<LocalModelServiceStatus>) => {
      if (pendingActionRef.current) {
        throw new Error(`Local model action already in progress: ${pendingActionRef.current}`);
      }
      pendingActionRef.current = key;
      setPendingAction(key);
      try {
        return await installStatus(await action());
      } finally {
        pendingActionRef.current = null;
        setPendingAction(null);
      }
    },
    [installStatus]
  );

  const install = useCallback(
    (id: string) => runAction(`install:${id}`, () => ipcBridge.managedModelService.local.install.invoke({ id })),
    [runAction]
  );

  const cancel = useCallback(
    (id: string) => runAction(`cancel:${id}`, () => ipcBridge.managedModelService.local.cancel.invoke({ id })),
    [runAction]
  );

  const remove = useCallback(
    (id: string) => runAction(`remove:${id}`, () => ipcBridge.managedModelService.local.remove.invoke({ id })),
    [runAction]
  );

  const setActive = useCallback(
    (id: string, enabled: boolean) =>
      runAction(`active:${id}`, () => ipcBridge.managedModelService.local.setActive.invoke({ id, enabled })),
    [runAction]
  );

  const refresh = useCallback(async () => {
    const [catalog, status] = await Promise.all([catalogQuery.mutate(), statusQuery.mutate()]);
    return { catalog, status };
  }, [catalogQuery, statusQuery]);

  return {
    catalog: catalogQuery.data,
    status: statusQuery.data,
    catalogError: catalogQuery.error,
    statusError: statusQuery.error,
    isLoading: catalogQuery.isLoading || statusQuery.isLoading,
    pendingAction,
    refresh,
    install,
    cancel,
    remove,
    setActive,
  };
};
