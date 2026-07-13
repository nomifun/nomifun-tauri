/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { ImageModelServiceStatus } from '@/common/types/provider/imageModelService';
import { MODEL_PROFILES_SWR_KEY } from '@/renderer/hooks/agent/useModelProfiles';
import { PROVIDERS_SWR_KEY } from '@/renderer/hooks/agent/useModelProviderList';
import { useCallback, useRef, useState } from 'react';
import useSWR, { mutate as mutateGlobal, type KeyedMutator } from 'swr';
import { isImageModelActivityPending } from './imageModelView';

export const LOCAL_IMAGE_MODEL_CATALOG_SWR_KEY = 'model-services/local/image/catalog';
export const LOCAL_IMAGE_MODEL_STATUS_SWR_KEY = 'model-services/local/image/status';

const fetchImageCatalog = () => ipcBridge.managedModelService.local.image.catalog.invoke();

let lastProjectionSignature = '';

const fetchImageStatus = async () => {
  const status = await ipcBridge.managedModelService.local.image.status.invoke();
  const projectionSignature = JSON.stringify({
    artifactsReady: status.artifactsReady,
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

/** Catalog, live progress, and serialized mutations for local image generation. */
export const useLocalImageModels = () => {
  const catalogQuery = useSWR(LOCAL_IMAGE_MODEL_CATALOG_SWR_KEY, fetchImageCatalog, {
    revalidateOnFocus: false,
    revalidateOnReconnect: true,
    shouldRetryOnError: false,
  });
  const statusQuery = useSWR<ImageModelServiceStatus>(LOCAL_IMAGE_MODEL_STATUS_SWR_KEY, fetchImageStatus, {
    revalidateOnFocus: false,
    revalidateOnReconnect: true,
    shouldRetryOnError: false,
    refreshInterval: (latestStatus) => (isImageModelActivityPending(latestStatus) ? 1_000 : 10_000),
  });
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const pendingActionRef = useRef<string | null>(null);
  const mutateStatus: KeyedMutator<ImageModelServiceStatus> = statusQuery.mutate;

  const installSnapshot = useCallback(
    async (status: ImageModelServiceStatus) => {
      await mutateStatus(status, false);
      await Promise.all([mutateGlobal(PROVIDERS_SWR_KEY), mutateGlobal(MODEL_PROFILES_SWR_KEY)]);
      return status;
    },
    [mutateStatus]
  );

  const runAction = useCallback(
    async (key: string, action: () => Promise<ImageModelServiceStatus>) => {
      if (pendingActionRef.current) {
        throw new Error(`Local image model action already in progress: ${pendingActionRef.current}`);
      }
      pendingActionRef.current = key;
      setPendingAction(key);
      try {
        return await installSnapshot(await action());
      } finally {
        pendingActionRef.current = null;
        setPendingAction(null);
      }
    },
    [installSnapshot]
  );

  const install = useCallback(
    (id: string) =>
      runAction(`install:${id}`, () => ipcBridge.managedModelService.local.image.install.invoke({ id })),
    [runAction]
  );

  const pause = useCallback(
    (id: string) => runAction(`pause:${id}`, () => ipcBridge.managedModelService.local.image.pause.invoke({ id })),
    [runAction]
  );

  const resume = useCallback(
    (id: string) =>
      runAction(`resume:${id}`, () => ipcBridge.managedModelService.local.image.resume.invoke({ id })),
    [runAction]
  );

  const remove = useCallback(
    (id: string) => runAction(`remove:${id}`, () => ipcBridge.managedModelService.local.image.remove.invoke({ id })),
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
    pause,
    resume,
    remove,
  };
};
