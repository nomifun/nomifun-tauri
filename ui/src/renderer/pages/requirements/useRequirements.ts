import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import { ipcBridge } from '@/common';
import { isHandledAuthExpiredHttpError } from '@/common/adapter/httpBridge';
import type { IListRequirementsParams, IRequirement } from '@/common/adapter/ipcBridge';
import {
  initialRequirementTagLoadState,
  reduceRequirementTagLoadState,
} from './requirementTagLoadState';

export function useRequirements(params: IListRequirementsParams) {
  const [items, setItems] = useState<IRequirement[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const paramsKey = JSON.stringify(params);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const res = await ipcBridge.requirements.list.invoke(params);
      setItems(res.items);
      setTotal(res.total);
      setError(null);
    } catch (e) {
      if (isHandledAuthExpiredHttpError(e)) return;
      console.error('Failed to load requirements', e);
      setError(String(e));
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [paramsKey]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Live updates: any create/update/status/delete invalidates the current view.
  // tagPaused also affects status/visibility (a paused tag's needs_review item
  // may have just appeared), so refresh on that too.
  useEffect(() => {
    const unsubs = [
      ipcBridge.requirements.onCreated.on(() => void refresh()),
      ipcBridge.requirements.onUpdated.on(() => void refresh()),
      ipcBridge.requirements.onStatusChanged.on(() => void refresh()),
      ipcBridge.requirements.onDeleted.on(() => void refresh()),
      ipcBridge.requirements.onTagPaused.on(() => void refresh()),
    ];
    return () => unsubs.forEach((u) => u());
  }, [refresh]);

  return { items, total, loading, error, refresh };
}

export function useRequirementTags() {
  const [state, dispatch] = useReducer(reduceRequirementTagLoadState, initialRequirementTagLoadState);
  const requestIdRef = useRef(0);
  const refresh = useCallback(async () => {
    const requestId = ++requestIdRef.current;
    dispatch({ type: 'start', requestId });
    try {
      const res = await ipcBridge.requirements.tags.invoke();
      dispatch({
        type: 'success',
        requestId,
        tags: res.map((tag) => ({ tag: tag.tag, done: tag.done, total: tag.total })),
      });
    } catch (e) {
      if (!isHandledAuthExpiredHttpError(e)) {
        console.error('Failed to load tags', e);
        dispatch({ type: 'failure', requestId, error: String(e) });
      }
    } finally {
      dispatch({ type: 'finish', requestId });
    }
  }, []);
  useEffect(() => {
    void refresh();
    const unsubs = [
      ipcBridge.requirements.onCreated.on(() => void refresh()),
      ipcBridge.requirements.onUpdated.on(() => void refresh()),
      ipcBridge.requirements.onStatusChanged.on(() => void refresh()),
      ipcBridge.requirements.onDeleted.on(() => void refresh()),
      ipcBridge.requirements.onTagPaused.on(() => void refresh()),
    ];
    return () => unsubs.forEach((u) => u());
  }, [refresh]);
  return { tags: state.tags, loading: state.loading, error: state.error, refresh };
}
