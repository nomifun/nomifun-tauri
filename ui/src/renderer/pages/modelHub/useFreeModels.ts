/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type {
  ManagedModelHealthBatchResult,
  ManagedModelHealthResult,
  ManagedModelServiceStatus,
} from '@/common/types/provider/managedModelService';
import { MODEL_PROFILES_SWR_KEY } from '@/renderer/hooks/agent/useModelProfiles';
import { PROVIDERS_SWR_KEY } from '@/renderer/hooks/agent/useModelProviderList';
import { useCallback, useEffect, useRef, useState } from 'react';
import useSWR, { mutate as mutateGlobal, type KeyedMutator, type SWRConfiguration } from 'swr';

export const FREE_MODEL_SERVICE_SWR_KEY = 'model-services/free/status';
export const FREE_MODEL_HEALTH_SWR_KEY = 'model-services/free/health';

const STATUS_SWR_OPTIONS: SWRConfiguration<ManagedModelServiceStatus, Error> = {
  revalidateOnFocus: false,
  revalidateOnReconnect: false,
  shouldRetryOnError: false,
  // The backend refresh task mutates the managed catalog independently of this
  // page. Lightweight polling keeps an already-open model hub and all provider
  // selectors converged without waiting for a user mutation or remount.
  refreshInterval: 60_000,
};

let lastFreeStatusSignature = '';

const fetchFreeStatus = async () => {
  const status = await ipcBridge.managedModelService.free.status.invoke();
  const signature = JSON.stringify({
    enabled: status.enabled,
    models: status.models,
    lastRefresh: status.lastRefresh,
    lastError: status.lastError,
  });
  if (lastFreeStatusSignature && signature !== lastFreeStatusSignature) {
    void mutateGlobal(PROVIDERS_SWR_KEY);
    void mutateGlobal(MODEL_PROFILES_SWR_KEY);
  }
  lastFreeStatusSignature = signature;
  return status;
};
const fetchFreeHealthSnapshot = () => ipcBridge.managedModelService.free.healthSnapshot.invoke();

/**
 * State/actions for the NomiFun-managed free-model service.
 *
 * Every mutation returns the complete latest status. We optimistically install
 * that response in the local cache and also refresh the provider projection so
 * all existing model selectors immediately see enable/catalog changes.
 */
export const useFreeModels = () => {
  const query = useSWR<ManagedModelServiceStatus>(FREE_MODEL_SERVICE_SWR_KEY, fetchFreeStatus, STATUS_SWR_OPTIONS);
  const healthQuery = useSWR<ManagedModelHealthResult[]>(
    FREE_MODEL_HEALTH_SWR_KEY,
    fetchFreeHealthSnapshot,
    {
      revalidateOnFocus: false,
      revalidateOnReconnect: false,
      shouldRetryOnError: false,
    }
  );
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const [healthResults, setHealthResults] = useState<Record<string, ManagedModelHealthResult>>({});
  const [healthCheckPending, setHealthCheckPending] = useState<'all' | string | null>(null);
  const healthCheckPendingRef = useRef<'all' | string | null>(null);
  const pendingActionRef = useRef<string | null>(null);
  const mutateStatus: KeyedMutator<ManagedModelServiceStatus> = query.mutate;

  const installStatus = useCallback(
    async (status: ManagedModelServiceStatus) => {
      await mutateStatus(status, false);
      await mutateGlobal(PROVIDERS_SWR_KEY);
      await mutateGlobal(MODEL_PROFILES_SWR_KEY);
      return status;
    },
    [mutateStatus]
  );

  const runAction = useCallback(
    async (key: string, action: () => Promise<ManagedModelServiceStatus>) => {
      if (pendingActionRef.current) {
        throw new Error(`Managed model action already in progress: ${pendingActionRef.current}`);
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

  const setServiceEnabled = useCallback(
    (enabled: boolean) =>
      runAction('service', () => ipcBridge.managedModelService.free.setEnabled.invoke({ enabled })),
    [runAction]
  );

  const refresh = useCallback(
    () => runAction('refresh', () => ipcBridge.managedModelService.free.refresh.invoke()),
    [runAction]
  );

  const setModelEnabled = useCallback(
    (id: string, enabled: boolean) =>
      runAction(`model:${id}`, () => ipcBridge.managedModelService.free.setModelEnabled.invoke({ id, enabled })),
    [runAction]
  );

  const installHealthResults = useCallback((results: ManagedModelHealthResult[]) => {
    setHealthResults((previous) => {
      const next = { ...previous };
      for (const result of results) next[result.modelId] = result;
      return next;
    });
  }, []);

  useEffect(() => {
    if (healthQuery.data) installHealthResults(healthQuery.data);
  }, [healthQuery.data, installHealthResults]);

  const checkAllHealth = useCallback(async (): Promise<ManagedModelHealthBatchResult> => {
    if (healthCheckPendingRef.current) {
      throw new Error(`Managed model health check already in progress: ${healthCheckPendingRef.current}`);
    }
    healthCheckPendingRef.current = 'all';
    setHealthCheckPending('all');
    try {
      const result = await ipcBridge.managedModelService.free.checkHealth.invoke();
      installHealthResults(result.results);
      return result;
    } finally {
      healthCheckPendingRef.current = null;
      setHealthCheckPending(null);
    }
  }, [installHealthResults]);

  const checkModelHealth = useCallback(
    async (id: string): Promise<ManagedModelHealthResult> => {
      if (healthCheckPendingRef.current) {
        throw new Error(`Managed model health check already in progress: ${healthCheckPendingRef.current}`);
      }
      healthCheckPendingRef.current = id;
      setHealthCheckPending(id);
      try {
        const result = await ipcBridge.managedModelService.free.checkModelHealth.invoke({ id });
        installHealthResults([result]);
        return result;
      } finally {
        healthCheckPendingRef.current = null;
        setHealthCheckPending(null);
      }
    },
    [installHealthResults]
  );

  return {
    status: query.data,
    error: query.error,
    isLoading: query.isLoading,
    mutate: mutateStatus,
    pendingAction,
    healthResults,
    healthCheckPending,
    refresh,
    setServiceEnabled,
    setModelEnabled,
    checkAllHealth,
    checkModelHealth,
  };
};
