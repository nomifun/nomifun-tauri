/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * useAssetLibrary — data + mutation controller for the asset-library panel (M4).
 *
 * Owns: paged listing (server-side `page`/`page_size`), debounced search,
 * kind + collection filters, the concurrent upload queue (progress + cancel),
 * and optimistic list mutations after upload / patch / delete. Kept UI-agnostic
 * so `AssetsPanel` stays a thin view.
 *
 * Collection filtering note: named collections filter server-side via the
 * `collection` query param; "Ungrouped" filters server-side too via the
 * `ungrouped=1` param (M10a — `collection IS NULL OR ''`). `displayItems` is
 * therefore just `items` (no client-side predicate). Because an ungrouped page
 * carries no named collections, the distinct-collection list is accumulated
 * across loads (`knownCollections`) so the collection dropdown never empties
 * while "Ungrouped" is selected.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';

import {
  createTextAsset,
  deleteAsset as apiDeleteAsset,
  listAssets,
  patchAsset as apiPatchAsset,
  uploadAsset,
  WORKSHOP_UPLOAD_ABORTED,
} from '../api';
import { revokeWorkshopMedia } from '../lib/media';
import type {
  AssetSortKey,
  CreateTextAssetBody,
  ListAssetsQuery,
  PatchAssetBody,
  WorkshopAsset,
  WorkshopAssetKind,
} from '../types';

export const ASSETS_PAGE_SIZE = 40;

/** Kind filter value — `all` plus the three concrete asset kinds. */
export type AssetKindFilter = 'all' | WorkshopAssetKind;

/** Collection filter sentinels + any named collection. */
export const COLLECTION_ALL = '__all__';
export const COLLECTION_UNGROUPED = '__ungrouped__';
export type CollectionFilter = typeof COLLECTION_ALL | typeof COLLECTION_UNGROUPED | string;

export type UploadStatus = 'uploading' | 'error' | 'done';

export interface UploadEntry {
  localId: string;
  fileName: string;
  percent: number;
  status: UploadStatus;
  /** Friendly error message key/text when `status === 'error'`. */
  error?: string;
  controller: AbortController;
}

let uploadSeq = 0;

export interface UseAssetLibrary {
  items: WorkshopAsset[];
  /** Items after applying the client-side "ungrouped" predicate. */
  displayItems: WorkshopAsset[];
  total: number;
  loading: boolean;
  loadingMore: boolean;
  error: string | null;
  hasMore: boolean;

  // filters
  query: string;
  setQuery: (q: string) => void;
  kind: AssetKindFilter;
  setKind: (k: AssetKindFilter) => void;
  collection: CollectionFilter;
  setCollection: (c: CollectionFilter) => void;
  /** Distinct collection names aggregated from loaded items (+ current selection). */
  collections: string[];
  /**
   * Result ordering. The in-canvas drawer leaves this at `'created_desc'`; the
   * platform Asset Library page drives it. Changing it reloads.
   */
  sort: AssetSortKey;
  setSort: (s: AssetSortKey) => void;
  /** Exact-match tag filter (asset-library page). `null` = no tag filter. */
  tag: string | null;
  setTag: (t: string | null) => void;
  isFiltering: boolean;
  clearFilters: () => void;

  reload: () => void;
  loadMore: () => void;

  // uploads
  uploads: UploadEntry[];
  startUploads: (files: File[]) => void;
  cancelUpload: (localId: string) => void;
  clearFinishedUploads: () => void;

  // mutations
  createText: (body: Omit<CreateTextAssetBody, 'kind'>) => Promise<WorkshopAsset>;
  patch: (id: string, patch: PatchAssetBody) => Promise<WorkshopAsset>;
  remove: (id: string) => Promise<void>;
}

export function useAssetLibrary(open: boolean): UseAssetLibrary {
  const [items, setItems] = useState<WorkshopAsset[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [query, setQuery] = useState('');
  const [debouncedQuery, setDebouncedQuery] = useState('');
  const [kind, setKind] = useState<AssetKindFilter>('all');
  const [collection, setCollection] = useState<CollectionFilter>(COLLECTION_ALL);
  // Ordering + tag filter — only the platform Asset Library page drives these;
  // the in-canvas drawer leaves them at their defaults (identical behavior to
  // before: `created_desc` maps to the backend's default order, no tag filter).
  const [sort, setSort] = useState<AssetSortKey>('created_desc');
  const [tag, setTag] = useState<string | null>(null);
  // Distinct collection names seen across every load — accumulated (grow-only)
  // so the collection dropdown keeps its options even while a filter (e.g.
  // "Ungrouped") narrows the current page to collection-less items.
  const [knownCollections, setKnownCollections] = useState<string[]>([]);

  const [uploads, setUploads] = useState<UploadEntry[]>([]);

  // Track whether the panel has ever opened, so we lazily load on first open.
  const openedRef = useRef(false);

  // ─── Debounce the search box ────────────────────────────────────────────────
  useEffect(() => {
    const h = window.setTimeout(() => setDebouncedQuery(query), 280);
    return () => window.clearTimeout(h);
  }, [query]);

  const buildQuery = useCallback(
    (nextPage: number): ListAssetsQuery => {
      const q: ListAssetsQuery = { in_library: true, page: nextPage, page_size: ASSETS_PAGE_SIZE, sort };
      if (kind !== 'all') q.kind = kind;
      const trimmed = debouncedQuery.trim();
      if (trimmed) q.q = trimmed;
      if (tag) q.tag = tag;
      // "Ungrouped" and named collections both filter server-side (mutually
      // exclusive on the wire).
      if (collection === COLLECTION_UNGROUPED) q.ungrouped = true;
      else if (collection !== COLLECTION_ALL) q.collection = collection;
      return q;
    },
    [kind, debouncedQuery, collection, sort, tag]
  );

  // Guard against out-of-order responses when filters change rapidly.
  const requestSeq = useRef(0);

  const load = useCallback(async () => {
    const seq = ++requestSeq.current;
    setLoading(true);
    setError(null);
    try {
      const res = await listAssets(buildQuery(1));
      if (seq !== requestSeq.current) return;
      setItems(res.items);
      setTotal(res.total);
      setPage(1);
    } catch (e) {
      if (seq !== requestSeq.current) return;
      setError(e instanceof Error ? e.message : String(e));
      setItems([]);
      setTotal(0);
    } finally {
      if (seq === requestSeq.current) setLoading(false);
    }
  }, [buildQuery]);

  const loadMore = useCallback(async () => {
    if (loading || loadingMore) return;
    if (items.length >= total) return;
    const seq = requestSeq.current;
    const nextPage = page + 1;
    setLoadingMore(true);
    try {
      const res = await listAssets(buildQuery(nextPage));
      if (seq !== requestSeq.current) return;
      setItems((prev) => {
        const seen = new Set(prev.map((a) => a.id));
        return [...prev, ...res.items.filter((a) => !seen.has(a.id))];
      });
      setTotal(res.total);
      setPage(nextPage);
    } catch {
      /* keep what we have; a subsequent scroll retries */
    } finally {
      if (seq === requestSeq.current) setLoadingMore(false);
    }
  }, [loading, loadingMore, items.length, total, page, buildQuery]);

  // Reload whenever filters change and the panel is open.
  useEffect(() => {
    if (!open) return;
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, kind, collection, debouncedQuery, sort, tag]);

  // First open: mark as opened (the effect above handles the initial load).
  useEffect(() => {
    if (open) openedRef.current = true;
  }, [open]);

  const hasMore = items.length < total;

  // ─── Aggregated collection list (server-filtered items → grow-only cache) ────
  // Merge any new collection names from the current page into the accumulator.
  // Returns the previous array unchanged when nothing is new, so this never
  // loops on its own `items` dependency.
  useEffect(() => {
    setKnownCollections((prev) => {
      const set = new Set(prev);
      let changed = false;
      for (const a of items) {
        if (a.collection && !set.has(a.collection)) {
          set.add(a.collection);
          changed = true;
        }
      }
      return changed ? [...set] : prev;
    });
  }, [items]);

  // `displayItems` is just `items` now that "Ungrouped" filters server-side;
  // kept in the API so `AssetsPanel` needs no change.
  const displayItems = items;

  const collections = useMemo(() => {
    const set = new Set<string>(knownCollections);
    for (const a of items) if (a.collection) set.add(a.collection);
    if (collection !== COLLECTION_ALL && collection !== COLLECTION_UNGROUPED) set.add(collection);
    return [...set].sort((a, b) => a.localeCompare(b));
  }, [items, collection, knownCollections]);

  const isFiltering = kind !== 'all' || collection !== COLLECTION_ALL || debouncedQuery.trim() !== '' || tag !== null;

  const clearFilters = useCallback(() => {
    setKind('all');
    setCollection(COLLECTION_ALL);
    setQuery('');
    setTag(null);
  }, []);

  // ─── Optimistic list mutations ──────────────────────────────────────────────
  const prependAsset = useCallback((asset: WorkshopAsset) => {
    setItems((prev) => (prev.some((a) => a.id === asset.id) ? prev : [asset, ...prev]));
    setTotal((n) => n + 1);
  }, []);

  const removeFromList = useCallback((id: string) => {
    setItems((prev) => {
      if (!prev.some((a) => a.id === id)) return prev;
      setTotal((n) => Math.max(0, n - 1));
      return prev.filter((a) => a.id !== id);
    });
  }, []);

  const replaceInList = useCallback((asset: WorkshopAsset) => {
    setItems((prev) => prev.map((a) => (a.id === asset.id ? asset : a)));
  }, []);

  // ─── Uploads ────────────────────────────────────────────────────────────────
  const startUploads = useCallback(
    (files: File[]) => {
      for (const file of files) {
        const localId = `up_${Date.now()}_${uploadSeq++}`;
        const controller = new AbortController();
        setUploads((prev) => [
          { localId, fileName: file.name, percent: 0, status: 'uploading', controller },
          ...prev,
        ]);
        void uploadAsset(file, {
          in_library: true,
          signal: controller.signal,
          onProgress: (percent) =>
            setUploads((prev) => prev.map((u) => (u.localId === localId ? { ...u, percent } : u))),
        })
          .then((asset) => {
            prependAsset(asset);
            // Drop the finished entry shortly after so the tray self-cleans.
            setUploads((prev) => prev.map((u) => (u.localId === localId ? { ...u, status: 'done', percent: 100 } : u)));
            window.setTimeout(() => {
              setUploads((prev) => prev.filter((u) => u.localId !== localId));
            }, 1200);
          })
          .catch((e: unknown) => {
            const raw = e instanceof Error ? e.message : String(e);
            const errorKey =
              raw === WORKSHOP_UPLOAD_ABORTED
                ? 'aborted'
                : raw === 'FILE_TOO_LARGE'
                  ? 'tooLarge'
                  : 'failed';
            if (errorKey === 'aborted') {
              setUploads((prev) => prev.filter((u) => u.localId !== localId));
            } else {
              setUploads((prev) =>
                prev.map((u) => (u.localId === localId ? { ...u, status: 'error', error: errorKey } : u))
              );
            }
          });
      }
    },
    [prependAsset]
  );

  const cancelUpload = useCallback((localId: string) => {
    setUploads((prev) => {
      const entry = prev.find((u) => u.localId === localId);
      entry?.controller.abort();
      return prev.filter((u) => u.localId !== localId);
    });
  }, []);

  const clearFinishedUploads = useCallback(() => {
    setUploads((prev) => prev.filter((u) => u.status === 'uploading'));
  }, []);

  // Abort any in-flight uploads when the panel unmounts.
  useEffect(() => {
    return () => {
      setUploads((prev) => {
        for (const u of prev) if (u.status === 'uploading') u.controller.abort();
        return prev;
      });
    };
  }, []);

  // ─── CRUD wrappers ──────────────────────────────────────────────────────────
  const createText = useCallback(
    async (body: Omit<CreateTextAssetBody, 'kind'>) => {
      const asset = await createTextAsset({ kind: 'text', ...body });
      prependAsset(asset);
      return asset;
    },
    [prependAsset]
  );

  const patch = useCallback(
    async (id: string, patchBody: PatchAssetBody) => {
      const asset = await apiPatchAsset(id, patchBody);
      // Moving out of the library removes it from this (library-scoped) list.
      if (asset.in_library === false) removeFromList(id);
      else replaceInList(asset);
      return asset;
    },
    [removeFromList, replaceInList]
  );

  const remove = useCallback(
    async (id: string) => {
      await apiDeleteAsset(id);
      removeFromList(id);
      revokeWorkshopMedia(id);
    },
    [removeFromList]
  );

  return {
    items,
    displayItems,
    total,
    loading,
    loadingMore,
    error,
    hasMore,
    query,
    setQuery,
    kind,
    setKind,
    collection,
    setCollection,
    collections,
    sort,
    setSort,
    tag,
    setTag,
    isFiltering,
    clearFilters,
    reload: load,
    loadMore,
    uploads,
    startUploads,
    cancelUpload,
    clearFinishedUploads,
    createText,
    patch,
    remove,
  };
}
