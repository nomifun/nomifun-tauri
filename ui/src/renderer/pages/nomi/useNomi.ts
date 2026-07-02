/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import { requestCompanionWindowSync } from '@renderer/hooks/useCompanionWindowsSync';
import type {
  ICompanionProfile,
  ICompanionProfilePatch,
  ICompanionSharedConfig,
  ICompanionSharedConfigPatch,
  ICompanionStatus,
  ICompanionWithStatus,
} from '@/common/adapter/ipcBridge';
import { useCallback, useEffect, useRef, useState } from 'react';

/** Optimistic RFC 7396-style merge of a shared-config patch (client mirror). */
const mergeSharedConfig = (prev: ICompanionSharedConfig, patch: ICompanionSharedConfigPatch): ICompanionSharedConfig => ({
  ...prev,
  ...(patch.collect ? { collect: { ...prev.collect, ...patch.collect } } : {}),
  ...(patch.learn ? { learn: { ...prev.learn, ...patch.learn } } : {}),
  ...(patch.evolve ? { evolve: { ...prev.evolve, ...patch.evolve } } : {}),
  ...(patch.archive ? { archive: { ...prev.archive, ...patch.archive } } : {}),
  ...(patch.smart_orchestration !== undefined ? { smart_orchestration: patch.smart_orchestration } : {}),
});

/** Optimistic RFC 7396-style merge of a companion-profile patch (client mirror). */
const mergeProfile = (prev: ICompanionProfile, patch: ICompanionProfilePatch): ICompanionProfile => ({
  ...prev,
  ...(patch.name !== undefined ? { name: patch.name } : {}),
  ...(patch.character !== undefined ? { character: patch.character } : {}),
  ...(patch.persona ? { persona: { ...prev.persona, ...patch.persona } } : {}),
  ...(patch.model ? { model: { ...prev.model, ...patch.model } } : {}),
  ...(patch.appearance ? { appearance: { ...prev.appearance, ...patch.appearance } } : {}),
});

/**
 * Shared (cross-companion) domain: collect/learn/default_companion_id config plus the
 * global counters (memories/suggestions — surfaced through the default companion's
 * status, since the memory store is shared).
 */
export const useCompanionShared = () => {
  const [sharedConfig, setSharedConfig] = useState<ICompanionSharedConfig | null>(null);
  const [status, setStatus] = useState<ICompanionStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const configRef = useRef<ICompanionSharedConfig | null>(null);
  configRef.current = sharedConfig;

  const refreshStatus = useCallback(async (defaultCompanionId?: string | null) => {
    const companionId = defaultCompanionId !== undefined ? defaultCompanionId : configRef.current?.default_companion_id;
    if (!companionId) {
      setStatus(null);
      return;
    }
    try {
      setStatus(await ipcBridge.companion.getCompanionStatus.invoke({ companion_id: companionId }));
    } catch {
      /* ignore — counters refresh on the next event */
    }
  }, []);

  const refresh = useCallback(async () => {
    try {
      const cfg = await ipcBridge.companion.getSharedConfig.invoke();
      setSharedConfig(cfg);
      await refreshStatus(cfg.default_companion_id);
    } finally {
      setLoading(false);
    }
  }, [refreshStatus]);

  useEffect(() => {
    void refresh();
    const refreshStats = () => void refreshStatus();
    const unsubs = [
      // Only shared-scope config changes belong to this hook.
      ipcBridge.companion.onConfigUpdated.on((evt) => {
        if (evt.scope === 'shared') void refresh();
      }),
      ipcBridge.companion.onLearnFinished.on(refreshStats),
      ipcBridge.companion.onSuggestionCreated.on(refreshStats),
      ipcBridge.companion.onSuggestionDecided.on(refreshStats),
      ipcBridge.companion.onMemoryCreated.on(refreshStats),
      // default_companion_id may be cleared/reassigned when a companion disappears.
      ipcBridge.companion.onCompanionDeleted.on(() => void refresh()),
    ];
    return () => unsubs.forEach((u) => u());
  }, [refresh, refreshStatus]);

  /**
   * Partial save (RFC 7396 merge patch) of the shared config. Applies the
   * patch optimistically so switches don't lag the round-trip; the server's
   * merged config (or a refresh on failure) reconciles.
   */
  const patchSharedConfig = useCallback(async (patch: ICompanionSharedConfigPatch) => {
    setSharedConfig((prev) => (prev ? mergeSharedConfig(prev, patch) : prev));
    try {
      const saved = await ipcBridge.companion.patchSharedConfig.invoke(patch);
      setSharedConfig(saved);
      return saved;
    } catch (e) {
      // Roll back the optimistic merge to the server's truth.
      void ipcBridge.companion.getSharedConfig.invoke().then(setSharedConfig).catch(() => {});
      throw e;
    }
  }, []);

  return { sharedConfig, status, loading, refresh, patchSharedConfig };
};

/** One companion's profile + status. Re-fetches when `companionId` changes.
 *  The loaded profile/status are bundled WITH the companion id they belong to
 *  (`data.id`), so a companion switch reads as null SYNCHRONOUSLY during render
 *  (see `fresh` below). Previously the reset lived in the effect (which runs
 *  post-commit), so a consumer keyed to the NEW companionId saw the PREVIOUS
 *  companion's profile/status for one render. That stale leak (a) made a fresh
 *  model-less companion look "model configured" → ChatTab fired
 *  ensureCompanionSession → 400「companion model not configured」, and (b) let the
 *  rail overlay rewrite the selected row's id/key → 侧栏切换疯狂复制. */
export const useCompanion = (companionId: string | null) => {
  const [data, setData] = useState<{
    id: string | null;
    profile: ICompanionProfile | null;
    status: ICompanionStatus | null;
  }>({ id: null, profile: null, status: null });
  const [loading, setLoading] = useState(Boolean(companionId));
  // Out-of-order guard: bumped on every companion switch / full refresh so a slow
  // stale response can't clobber the newer companion's data.
  const seqRef = useRef(0);

  const refresh = useCallback(async () => {
    const seq = ++seqRef.current;
    if (!companionId) {
      setData({ id: null, profile: null, status: null });
      setLoading(false);
      return;
    }
    try {
      const p = await ipcBridge.companion.getCompanion.invoke({ companion_id: companionId });
      if (seq !== seqRef.current) return;
      const { status: st, ...prof } = p;
      setData({ id: companionId, profile: prof, status: st });
    } finally {
      if (seq === seqRef.current) setLoading(false);
    }
  }, [companionId]);

  const refreshStatus = useCallback(async () => {
    if (!companionId) return;
    const seq = seqRef.current;
    try {
      const st = await ipcBridge.companion.getCompanionStatus.invoke({ companion_id: companionId });
      if (seq === seqRef.current) setData((prev) => (prev.id === companionId ? { ...prev, status: st } : prev));
    } catch {
      /* ignore */
    }
  }, [companionId]);

  useEffect(() => {
    // Drop any in-flight response for the previous companion, then reload. The
    // synchronous `fresh` gate (below) already hides the previous companion's
    // data this render; this just prevents a late response writing under the
    // new id.
    seqRef.current++;
    setLoading(Boolean(companionId));
    void refresh();
    if (!companionId) return;
    const refreshStats = () => void refreshStatus();
    const unsubs = [
      // Per-companion scope only; shared-scope changes are useCompanionShared's business.
      ipcBridge.companion.onConfigUpdated.on((evt) => {
        if (evt.scope === companionId || evt.companion_id === companionId) void refresh();
      }),
      ipcBridge.companion.onMoodChanged.on(refreshStats),
      ipcBridge.companion.onLearnFinished.on(refreshStats),
      ipcBridge.companion.onSuggestionCreated.on(refreshStats),
      ipcBridge.companion.onSuggestionDecided.on(refreshStats),
      ipcBridge.companion.onMemoryCreated.on(refreshStats),
    ];
    return () => unsubs.forEach((u) => u());
  }, [companionId, refresh, refreshStatus]);

  /**
   * Partial save (RFC 7396 merge patch) of this companion's profile, applied
   * optimistically (mirrors the legacy patchConfig behavior).
   */
  const patchCompanion = useCallback(
    async (patch: ICompanionProfilePatch) => {
      if (!companionId) return undefined;
      setData((prev) =>
        prev.id === companionId && prev.profile ? { ...prev, profile: mergeProfile(prev.profile, patch) } : prev
      );
      try {
        const saved = await ipcBridge.companion.patchCompanion.invoke({ companion_id: companionId, patch });
        setData((prev) => (prev.id === companionId ? { ...prev, profile: saved } : prev));
        // Toggling 桌面显示 (appearance.companion_enabled) must reconcile the native
        // pet window NOW — directly, not via the lossy WS config-updated echo that
        // intermittently dropped (→ "点隐藏/显示要右键刷新才生效"). No-op outside the
        // Tauri desktop shell.
        if (patch.appearance && patch.appearance.companion_enabled !== undefined) {
          requestCompanionWindowSync();
        }
        // A model change flips the backend-derived status.model_configured and
        // mints/propagates the companion session. Refresh status now so 总览's
        // 「未配置模型」warning clears and ChatTab activates the session promptly,
        // instead of waiting on the config-updated WS echo.
        if (patch.model) void refreshStatus();
        return saved;
      } catch (e) {
        // Roll back the optimistic merge to the server's truth.
        void refresh();
        throw e;
      }
    },
    [companionId, refresh, refreshStatus]
  );

  // Only expose data that belongs to the CURRENT companionId — the synchronous
  // reset that makes a switch null-out profile/status in the same render.
  const fresh = data.id === companionId;
  return {
    profile: fresh ? data.profile : null,
    status: fresh ? data.status : null,
    loading: fresh ? loading : Boolean(companionId),
    refresh,
    refreshStatus,
    patchCompanion,
  };
};

/** The companion roster (profiles + statuses), kept fresh via WS events. */
export const useCompanions = () => {
  const [companions, setCompanions] = useState<ICompanionWithStatus[]>([]);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      setCompanions(await ipcBridge.companion.listCompanions.invoke());
    } finally {
      setLoading(false);
    }
  }, []);

  /** Incremental refresh of a single roster row (insert-or-replace). */
  const refreshOne = useCallback(
    async (companionId: string) => {
      // Guard against an undefined/empty id (e.g. a companion event fired mid-create
      // before the row has an id) — `/api/companion/companions/undefined` would 404-noise.
      if (!companionId) return;
      try {
        const p = await ipcBridge.companion.getCompanion.invoke({ companion_id: companionId });
        setCompanions((prev) => {
          const idx = prev.findIndex((x) => x.id === p.id);
          if (idx === -1) return [...prev, p];
          const next = prev.slice();
          next[idx] = p;
          return next;
        });
      } catch {
        // Row may be gone (deleted between event and fetch) — resync the list.
        void refresh();
      }
    },
    [refresh]
  );

  useEffect(() => {
    void refresh();
    const unsubs = [
      ipcBridge.companion.onCompanionCreated.on((evt) => void refreshOne(evt.companion_id)),
      ipcBridge.companion.onCompanionDeleted.on((evt) => setCompanions((prev) => prev.filter((p) => p.id !== evt.companion_id))),
      ipcBridge.companion.onConfigUpdated.on((evt) => {
        const pid = evt.companion_id ?? (evt.scope && evt.scope !== 'shared' ? evt.scope : undefined);
        if (pid) void refreshOne(pid);
      }),
      // XP/level moves after a learn run — refresh the badges.
      ipcBridge.companion.onLearnFinished.on(() => void refresh()),
    ];
    return () => unsubs.forEach((u) => u());
  }, [refresh, refreshOne]);

  return { companions, loading, refresh };
};
