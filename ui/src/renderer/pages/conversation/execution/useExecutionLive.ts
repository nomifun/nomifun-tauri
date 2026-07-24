import { ipcBridge } from '@/common';
import type { TAgentExecutionDetail } from '@/common/types/agentExecution/agentExecutionTypes';
import { useCallback, useEffect, useRef, useState } from 'react';
import type { ExecutionId } from '@/common/types/ids';

const EVENT_REFETCH_DEBOUNCE_MS = 180;

export function useExecutionLive(executionId: ExecutionId | undefined): {
  detail: TAgentExecutionDetail | null;
  loading: boolean;
  refetch: () => Promise<void>;
} {
  const [detail, setDetail] = useState<TAgentExecutionDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const requestSequence = useRef(0);
  const appliedEventSequence = useRef(0);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const refetch = useCallback(async () => {
    if (!executionId) {
      requestSequence.current += 1;
      setDetail(null);
      setLoading(false);
      return;
    }
    const sequence = ++requestSequence.current;
    setLoading(true);
    try {
      const next = await ipcBridge.agentExecution.get.invoke({
        execution_id: executionId,
      });
      if (sequence === requestSequence.current) {
        appliedEventSequence.current = next?.execution.event_sequence ?? 0;
        setDetail(next ?? null);
      }
    } catch (error) {
      console.error('[useExecutionLive] Failed to fetch execution detail:', error);
      if (sequence === requestSequence.current) setDetail(null);
    } finally {
      if (sequence === requestSequence.current) setLoading(false);
    }
  }, [executionId]);

  useEffect(() => {
    appliedEventSequence.current = 0;
    void refetch();
  }, [refetch]);

  useEffect(() => {
    if (!executionId) return;
    const unsubscribe = ipcBridge.agentExecution.events.changed.on((event) => {
      if (event.execution_id !== executionId) return;
      if (event.sequence <= appliedEventSequence.current) return;
      if (timer.current !== null) clearTimeout(timer.current);
      timer.current = setTimeout(() => {
        timer.current = null;
        void refetch();
      }, EVENT_REFETCH_DEBOUNCE_MS);
    });
    return () => {
      unsubscribe();
      if (timer.current !== null) clearTimeout(timer.current);
      timer.current = null;
    };
  }, [executionId, refetch]);

  return { detail, loading, refetch };
}
