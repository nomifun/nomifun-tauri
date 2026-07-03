/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useCallback, useEffect, useState } from 'react';
import { ipcBridge } from '@/common';
import type { IPublicAgent, IPublicAgentPatch } from '@/common/adapter/ipcBridge';
import { useModelProviderList } from '@renderer/hooks/agent/useModelProviderList';

/**
 * 对外伙伴（Public Companion）花名册 —— 面向陌生人的企业级客服 agent 列表 + 创建。
 *
 * 与「桌面伙伴（desktop companion）」完全独立：独立数据 / 配置 / 控制台，绝不混入桌面伙伴
 * 列表或会话侧边栏。数据经 `/api/public-agents` REST 契约拉取。
 */
export const usePublicAgents = () => {
  const [agents, setAgents] = useState<IPublicAgent[]>([]);
  const [loading, setLoading] = useState(true);
  const { providers, getAvailableModels } = useModelProviderList();

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setAgents((await ipcBridge.publicAgent.list.invoke()) ?? []);
    } catch {
      // Backend may not have shipped the endpoint yet — degrade to an empty roster.
      setAgents([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const create = useCallback(
    async (name: string): Promise<IPublicAgent> => {
      const created = await ipcBridge.publicAgent.create.invoke({ name });
      // 开箱即用: seed the new agent's model from the machine's first available
      // provider/model so it can answer strangers immediately (the owner can
      // change it under 身份 & 话术). Best-effort — a missing model just means
      // the owner must pick one in the console before the agent can serve.
      if (!created.model?.provider_id) {
        const provider = providers[0];
        const model = provider ? (getAvailableModels(provider)[0] ?? '') : '';
        if (provider && model) {
          try {
            await ipcBridge.publicAgent.patch.invoke({
              id: created.id,
              patch: { model: { provider_id: provider.id, model } },
            });
          } catch {
            // Non-fatal: the console model picker remains the authoritative path.
          }
        }
      }
      await refresh();
      return created;
    },
    [providers, getAvailableModels, refresh]
  );

  return { agents, loading, refresh, create };
};

/**
 * 单个对外伙伴的档案 + 乐观 PATCH 通道。乐观更新本地状态，失败则回读权威值。
 */
export const usePublicAgent = (id: string | null) => {
  const [agent, setAgent] = useState<IPublicAgent | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    if (!id) {
      setAgent(null);
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      setAgent(await ipcBridge.publicAgent.get.invoke({ id }));
    } catch {
      setAgent(null);
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const patch = useCallback(
    async (p: IPublicAgentPatch): Promise<IPublicAgent | undefined> => {
      if (!id) return undefined;
      // Optimistic merge (model is a nested object → shallow-merge it too).
      setAgent((prev) =>
        prev
          ? { ...prev, ...p, model: p.model ? { ...prev.model, ...p.model } : prev.model }
          : prev
      );
      try {
        const updated = await ipcBridge.publicAgent.patch.invoke({ id, patch: p });
        setAgent(updated);
        return updated;
      } catch (e) {
        // Re-sync to the authoritative record so the UI never lies after a failed save.
        await load();
        throw e;
      }
    },
    [id, load]
  );

  return { agent, loading, reload: load, patch };
};
