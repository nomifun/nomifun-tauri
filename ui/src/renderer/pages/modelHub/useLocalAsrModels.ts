/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { AsrModelServiceStatus } from '@/common/types/provider/asrModelService';
import { LOCAL_ASR_STATUS_CHANGED_EVENT } from '@/renderer/services/localAsrEvents';
import { useCallback, useRef, useState } from 'react';
import useSWR, { type KeyedMutator } from 'swr';
import { isLocalModelActivityPending } from './localModelView';

export const LOCAL_ASR_CATALOG_SWR_KEY = 'model-services/local/asr/catalog';
export const LOCAL_ASR_STATUS_SWR_KEY = 'model-services/local/asr/status';

const fetchLocalAsrCatalog = () => ipcBridge.managedModelService.local.asr.catalog.invoke();
const fetchLocalAsrStatus = () => ipcBridge.managedModelService.local.asr.status.invoke();

const notifyLocalAsrStatusChanged = (): void => {
  if (typeof window !== 'undefined') {
    window.dispatchEvent(new CustomEvent(LOCAL_ASR_STATUS_CHANGED_EVENT));
  }
};

export const localAsrSpeechInputStateKey = (status: AsrModelServiceStatus): string =>
  [status.enabled ? 'enabled' : 'disabled', status.ready ? 'ready' : 'not-ready', status.activeModelId ?? 'none'].join(
    ':'
  );

/** State and serialized mutations for the local one-shot speech recognizer. */
export const useLocalAsrModels = () => {
  const lastSpeechInputStateKeyRef = useRef<string | null>(null);
  const observeStatus = useCallback((status: AsrModelServiceStatus) => {
    const nextKey = localAsrSpeechInputStateKey(status);
    if (lastSpeechInputStateKeyRef.current === nextKey) {
      return;
    }
    lastSpeechInputStateKeyRef.current = nextKey;
    notifyLocalAsrStatusChanged();
  }, []);
  const catalogQuery = useSWR(LOCAL_ASR_CATALOG_SWR_KEY, fetchLocalAsrCatalog, {
    revalidateOnFocus: false,
    revalidateOnReconnect: true,
    shouldRetryOnError: false,
  });
  const statusQuery = useSWR<AsrModelServiceStatus>(LOCAL_ASR_STATUS_SWR_KEY, fetchLocalAsrStatus, {
    revalidateOnFocus: false,
    revalidateOnReconnect: true,
    shouldRetryOnError: false,
    refreshInterval: (latestStatus) => (isLocalModelActivityPending(latestStatus) ? 1_000 : 10_000),
    onSuccess: observeStatus,
  });
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const pendingActionRef = useRef<string | null>(null);
  const mutateStatus: KeyedMutator<AsrModelServiceStatus> = statusQuery.mutate;

  const installStatus = useCallback(
    async (status: AsrModelServiceStatus) => {
      await mutateStatus(status, false);
      observeStatus(status);
      return status;
    },
    [mutateStatus, observeStatus]
  );

  const runAction = useCallback(
    async (key: string, action: () => Promise<AsrModelServiceStatus>) => {
      if (pendingActionRef.current) {
        throw new Error(`Local ASR model action already in progress: ${pendingActionRef.current}`);
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
    (id: string) => runAction(`install:${id}`, () => ipcBridge.managedModelService.local.asr.install.invoke({ id })),
    [runAction]
  );

  const cancel = useCallback(
    (id: string) => runAction(`cancel:${id}`, () => ipcBridge.managedModelService.local.asr.cancel.invoke({ id })),
    [runAction]
  );

  const remove = useCallback(
    (id: string) => runAction(`remove:${id}`, () => ipcBridge.managedModelService.local.asr.remove.invoke({ id })),
    [runAction]
  );

  const setActive = useCallback(
    (id: string, enabled: boolean) =>
      runAction(`active:${id}`, () => ipcBridge.managedModelService.local.asr.setActive.invoke({ id, enabled })),
    [runAction]
  );

  const refresh = useCallback(async () => {
    const [catalog, status] = await Promise.all([catalogQuery.mutate(), statusQuery.mutate()]);
    if (status) {
      observeStatus(status);
    }
    return { catalog, status };
  }, [catalogQuery, observeStatus, statusQuery]);

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
