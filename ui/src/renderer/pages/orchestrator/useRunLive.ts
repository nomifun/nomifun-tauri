/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { ipcBridge } from '@/common';
import type { TRunDetail } from '@/common/types/orchestrator/orchestratorTypes';
import { useCallback, useEffect, useRef, useState } from 'react';

/** Trailing debounce for EVENT-driven refetches (ms). A run-engine event burst
 * (task assigned + status changed + plan updated landing together) collapses
 * into ONE `runs.get` instead of one per event. Short enough that the canvas
 * still reads as real-time; explicit `refetch()` calls (approve / rerun /
 * adopt) bypass it entirely. */
const EVENT_REFETCH_DEBOUNCE_MS = 180;

/**
 * Live view of a single orchestration run. Fetches the full run detail
 * (`run` + plan tasks/deps + assignments) over REST, then refetches whenever
 * any of the five run-engine WebSocket events for THIS run arrives. Every event
 * payload carries `run_id`, so we filter on it before refetching.
 *
 * Passing `undefined` clears the detail and skips subscriptions (e.g. when no
 * run is selected).
 *
 * 性能（agent 集群需求3）：事件驱动的 refetch 走 trailing 去抖 + 在飞合并——
 * 去抖窗口内的事件并成一次 REST；fetch 在飞时再来事件只置 dirty，返场后补一次。
 * 显式 `refetch()`（approve/rerun/adopt 等操作路径显式 await）不去抖、立即拉，
 * 过期竞态由自增序号守卫（旧响应绝不覆盖新数据）。
 */
export function useRunLive(runId: string | undefined): {
  detail: TRunDetail | null;
  loading: boolean;
  refetch: () => Promise<void>;
} {
  const [detail, setDetail] = useState<TRunDetail | null>(null);
  const [loading, setLoading] = useState(false);

  // 去抖/合并状态全放 ref：高频事件处理自身绝不触发重渲染。
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const inFlightRef = useRef(false);
  const dirtyRef = useRef(false);
  // 自增请求序号：仅“最新一次”请求的结果允许落地，关闭乱序覆盖窗口。
  const seqRef = useRef(0);

  const refetch = useCallback(async () => {
    if (!runId) {
      seqRef.current += 1; // 使任何在飞的旧请求失效
      setDetail(null);
      return;
    }
    const seq = ++seqRef.current;
    inFlightRef.current = true;
    setLoading(true);
    try {
      const result = await ipcBridge.orchestrator.runs.get.invoke({ id: runId });
      if (seq === seqRef.current) setDetail(result ?? null);
    } catch (err) {
      console.error('[useRunLive] Failed to fetch run detail:', err);
      if (seq === seqRef.current) setDetail(null);
    } finally {
      if (seq === seqRef.current) {
        inFlightRef.current = false;
        setLoading(false);
      }
    }
  }, [runId]);

  // Initial fetch (and clear when runId becomes undefined).
  useEffect(() => {
    void refetch();
  }, [refetch]);

  // Refetch when any run-engine event for this run arrives — debounced +
  // in-flight-merged. All five payloads carry `run_id`; we filter on it and
  // collect every unsubscribe (plus the pending timer) for cleanup.
  useEffect(() => {
    if (!runId) return;
    let disposed = false;

    const runDebounced = async () => {
      await refetch();
      // fetch 在飞期间又有事件到达 → 补一次（仍走去抖，持续风暴下形成稳定节流）。
      if (!disposed && dirtyRef.current) {
        dirtyRef.current = false;
        schedule();
      }
    };

    const schedule = () => {
      if (disposed) return;
      if (inFlightRef.current) {
        dirtyRef.current = true;
        return;
      }
      if (timerRef.current !== null) return; // 已排队 → 并入本次
      timerRef.current = setTimeout(() => {
        timerRef.current = null;
        void runDebounced();
      }, EVENT_REFETCH_DEBOUNCE_MS);
    };

    const onRunEvent = (e: { run_id: string }) => {
      if (e.run_id === runId) schedule();
    };
    const unsubs = [
      ipcBridge.orchestrator.runEvents.statusChanged.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.planUpdated.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.completed.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.taskStatusChanged.on(onRunEvent),
      ipcBridge.orchestrator.runEvents.taskAssigned.on(onRunEvent),
    ];
    return () => {
      disposed = true;
      for (const unsub of unsubs) {
        unsub();
      }
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      dirtyRef.current = false;
    };
  }, [runId, refetch]);

  return { detail, loading, refetch };
}
