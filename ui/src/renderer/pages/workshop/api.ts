/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Creative Workshop REST client.
 *
 * Talks to `nomifun-workshop` (`/api/workshop/*`) and `nomifun-creation`
 * (`/api/creation/*`) over the same HTTP channel the rest of the app uses:
 * `httpRequest` from the shared `httpBridge` (base-URL resolution for desktop vs
 * WebUI, local-trust / CSRF headers, `{ success, data }` envelope unwrapping, and
 * structured `BackendHttpError`s). JSON endpoints go through `httpRequest`;
 * multipart upload needs a raw `XMLHttpRequest` for progress + abort, mirroring
 * `services/FileService.uploadFileViaHttp`.
 *
 * Frozen at M0 (append-only): downstream modules may add functions/fields but
 * must not change existing signatures.
 */

import { buildBackendAuthHeaders, getBaseUrl, httpRequest } from '@/common/adapter/httpBridge';
import {
  parseAssetId,
  parseCanvasId,
  parseCreationTaskId,
  parseProviderId,
  parseWorkshopEdgeId,
  parseWorkshopNodeId,
} from '@/common/types/ids';
import type { AssetId, CanvasId, CreationTaskId } from '@/common/types/ids';
import type {
  CanvasDetailResponse,
  CreateCanvasBody,
  CreateTaskBody,
  CreateTextAssetBody,
  CreationTask,
  ListAssetsQuery,
  ListAssetsResponse,
  ListTasksQuery,
  PatchAssetBody,
  PatchCanvasBody,
  PutDocResponse,
  UploadAssetOptions,
  WorkshopAsset,
  WorkshopCanvasDoc,
  WorkshopCanvasMeta,
  WorkshopNode,
} from './types';

// ─────────────────────────────────────────────────────────────────────────────
// URL helpers
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Resolve a backend-relative serve path (e.g. an asset's `url` /
 * `thumb_url`, or a canvas `thumbnail_url`) to an absolute URL usable in
 * `<img src>` / `<video src>`.
 *
 * The backend returns same-origin relative paths like `/api/workshop/files/{id}`.
 * WebUI (same-origin) can use them verbatim, but the desktop webview must prefix
 * the loopback backend origin. Absolute URLs (or empty values) are passed through.
 */
export function resolveWorkshopUrl(path: string | null | undefined): string | null {
  if (!path) return null;
  if (/^(https?:|blob:|data:)/i.test(path)) return path;
  const base = getBaseUrl();
  return path.startsWith('/') ? `${base}${path}` : `${base}/${path}`;
}

/** Build the binary serve URL for an asset (optionally its thumbnail). */
export function workshopFileUrl(assetId: AssetId, thumb = false): string {
  const suffix = thumb ? '?thumb=1' : '';
  return `${getBaseUrl()}/api/workshop/files/${encodeURIComponent(assetId)}${suffix}`;
}

function normalizeCanvasMeta(meta: WorkshopCanvasMeta): WorkshopCanvasMeta {
  return { ...meta, id: parseCanvasId(meta.id) };
}

function normalizeAsset(asset: WorkshopAsset): WorkshopAsset {
  const origin = asset.origin
    ? {
        ...asset.origin,
        provider_id:
          asset.origin.provider_id === undefined ? undefined : parseProviderId(asset.origin.provider_id),
        canvas_id: asset.origin.canvas_id === undefined ? undefined : parseCanvasId(asset.origin.canvas_id),
        node_id:
          asset.origin.node_id === undefined ? undefined : parseWorkshopNodeId(asset.origin.node_id),
        task_id:
          asset.origin.task_id === undefined ? undefined : parseCreationTaskId(asset.origin.task_id),
      }
    : null;
  return { ...asset, id: parseAssetId(asset.id), origin };
}

function normalizeNode(node: WorkshopNode): WorkshopNode {
  const data = { ...(node.data as Record<string, unknown>) };
  if ((node.kind === 'image' || node.kind === 'video') && data.assetId !== null) {
    data.assetId = parseAssetId(data.assetId);
  }
  if (node.kind === 'generator') {
    if (data.providerId !== undefined) data.providerId = parseProviderId(data.providerId);
    if (data.taskId !== undefined && data.taskId !== null) data.taskId = parseCreationTaskId(data.taskId);
    if (!Array.isArray(data.resultAssetIds)) {
      throw new TypeError('Invalid workshop generator resultAssetIds');
    }
    data.resultAssetIds = data.resultAssetIds.map(parseAssetId);
    if (data.maskAssetId !== undefined) data.maskAssetId = parseAssetId(data.maskAssetId);
    if (data.batch && typeof data.batch === 'object' && 'primary' in data.batch) {
      const batch = { ...(data.batch as Record<string, unknown>) };
      if (batch.primary !== undefined) batch.primary = parseAssetId(batch.primary);
      data.batch = batch;
    }
  }
  return {
    ...node,
    id: parseWorkshopNodeId(node.id),
    groupId: node.groupId == null ? node.groupId : parseWorkshopNodeId(node.groupId),
    data,
  } as WorkshopNode;
}

function normalizeCanvasDoc(doc: WorkshopCanvasDoc): WorkshopCanvasDoc {
  return {
    ...doc,
    nodes: doc.nodes.map(normalizeNode),
    edges: doc.edges.map((edge) => ({
      ...edge,
      id: parseWorkshopEdgeId(edge.id),
      from: parseWorkshopNodeId(edge.from),
      to: parseWorkshopNodeId(edge.to),
    })),
  };
}

function normalizeCreationTask(task: CreationTask): CreationTask {
  return {
    ...task,
    id: parseCreationTaskId(task.id),
    canvas_id: task.canvas_id == null ? null : parseCanvasId(task.canvas_id),
    node_id: task.node_id == null ? null : parseWorkshopNodeId(task.node_id),
    provider_id: parseProviderId(task.provider_id),
    result_asset_ids: task.result_asset_ids.map(parseAssetId),
  };
}

/** Serialize a params bag into a `?a=b&c=d` string, skipping undefined/null. */
function queryString(params: Record<string, string | number | boolean | undefined | null>): string {
  const sp = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === '') continue;
    sp.append(key, String(value));
  }
  const s = sp.toString();
  return s ? `?${s}` : '';
}

// ─────────────────────────────────────────────────────────────────────────────
// §3.1 Canvases
// ─────────────────────────────────────────────────────────────────────────────

/** List all canvases (backend returns them ordered by `updated_at` desc). */
export async function listCanvases(): Promise<WorkshopCanvasMeta[]> {
  const res = await httpRequest<{ canvases: WorkshopCanvasMeta[] }>('GET', '/api/workshop/canvases');
  return (res?.canvases ?? []).map(normalizeCanvasMeta);
}

/** Create a canvas (defaults its title backend-side when omitted) and jump straight in. */
export async function createCanvas(body: CreateCanvasBody = {}): Promise<WorkshopCanvasMeta> {
  return normalizeCanvasMeta(await httpRequest<WorkshopCanvasMeta>('POST', '/api/workshop/canvases', body));
}

/** Load a canvas's index row plus its full (opaque) doc. */
export async function getCanvas(id: CanvasId): Promise<CanvasDetailResponse> {
  const detail = await httpRequest<CanvasDetailResponse>('GET', `/api/workshop/canvases/${encodeURIComponent(id)}`);
  return { meta: normalizeCanvasMeta(detail.meta), doc: normalizeCanvasDoc(detail.doc) };
}

/** Persist the canvas doc (atomic write; backend re-derives `node_count`). */
export async function putCanvasDoc(id: CanvasId, doc: WorkshopCanvasDoc): Promise<PutDocResponse> {
  return httpRequest<PutDocResponse>('PUT', `/api/workshop/canvases/${encodeURIComponent(id)}/doc`, { doc });
}

/** Rename a canvas. */
export async function patchCanvas(id: CanvasId, patch: PatchCanvasBody): Promise<WorkshopCanvasMeta> {
  return normalizeCanvasMeta(
    await httpRequest<WorkshopCanvasMeta>('PATCH', `/api/workshop/canvases/${encodeURIComponent(id)}`, patch)
  );
}

/** Delete a canvas (index row + domain directory; assets are GC'd separately). */
export async function deleteCanvas(id: CanvasId): Promise<void> {
  await httpRequest<void>('DELETE', `/api/workshop/canvases/${encodeURIComponent(id)}`);
}

// ─────────────────────────────────────────────────────────────────────────────
// §3.2 Assets
// ─────────────────────────────────────────────────────────────────────────────

/** Query the asset library with optional kind/collection/search/pagination filters. */
export async function listAssets(query: ListAssetsQuery = {}): Promise<ListAssetsResponse> {
  const qs = queryString({
    kind: query.kind,
    collection: query.collection,
    q: query.q,
    in_library: query.in_library,
    ungrouped: query.ungrouped,
    tag: query.tag,
    sort: query.sort,
    page: query.page,
    page_size: query.page_size,
  });
  const res = await httpRequest<ListAssetsResponse>('GET', `/api/workshop/assets${qs}`);
  return { items: (res?.items ?? []).map(normalizeAsset), total: res?.total ?? 0 };
}

/** Register a text asset (or existing content) in the library. */
export async function createTextAsset(body: CreateTextAssetBody): Promise<WorkshopAsset> {
  return normalizeAsset(await httpRequest<WorkshopAsset>('POST', '/api/workshop/assets', body));
}

/** Partially edit an asset's metadata (title/collection/tags/in_library). */
export async function patchAsset(id: AssetId, patch: PatchAssetBody): Promise<WorkshopAsset> {
  return normalizeAsset(
    await httpRequest<WorkshopAsset>('PATCH', `/api/workshop/assets/${encodeURIComponent(id)}`, patch)
  );
}

/** Delete an asset (index row + on-disk file). */
export async function deleteAsset(id: AssetId): Promise<void> {
  await httpRequest<void>('DELETE', `/api/workshop/assets/${encodeURIComponent(id)}`);
}

/**
 * Bulk-rename a collection across every asset that used it (asset-library
 * management). A blank `to` ungroups the affected assets. Returns rows updated.
 */
export async function renameCollection(from: string, to: string): Promise<number> {
  const res = await httpRequest<{ updated: number }>('POST', '/api/workshop/collections/rename', {
    from,
    to,
  });
  return res?.updated ?? 0;
}

/** Options for {@link uploadAsset} — metadata plus progress / cancellation hooks. */
export interface UploadAssetHooks extends UploadAssetOptions {
  /** Receives upload percentage (0-100). */
  onProgress?: (percent: number) => void;
  /** Cancel the upload; aborting the XHR frees the backend connection. */
  signal?: AbortSignal;
}

/** Sentinel error message when an upload is cancelled by the caller. */
export const WORKSHOP_UPLOAD_ABORTED = 'Workshop upload aborted';

/**
 * Upload a binary asset via HTTP multipart.
 *
 * Uses a raw `XMLHttpRequest` (not `httpRequest`) so callers get upload-progress
 * events and abort support. Because the raw XHR bypasses both `httpRequest`'s
 * header logic and the desktop shell's `window.fetch` interceptor, we apply the
 * trust (desktop) / CSRF (WebUI) headers ourselves — otherwise the guarded
 * endpoint rejects the request with 403. The response envelope (`{ success, data }`
 * or a raw object) is unwrapped to a {@link WorkshopAsset}.
 */
export function uploadAsset(file: File, hooks: UploadAssetHooks = {}): Promise<WorkshopAsset> {
  const { title, collection, tags, in_library, onProgress, signal } = hooks;

  const formData = new FormData();
  formData.append('file', file);
  if (title) formData.append('title', title);
  if (collection) formData.append('collection', collection);
  if (tags && tags.length > 0) formData.append('tags', JSON.stringify(tags));
  if (in_library !== undefined) formData.append('in_library', in_library ? '1' : '0');

  return new Promise<WorkshopAsset>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${getBaseUrl()}/api/workshop/assets/upload`);

    for (const [name, value] of Object.entries(buildBackendAuthHeaders('POST'))) {
      xhr.setRequestHeader(name, value);
    }

    let onSignalAbort: (() => void) | null = null;
    if (signal) {
      if (signal.aborted) {
        reject(new Error(WORKSHOP_UPLOAD_ABORTED));
        return;
      }
      onSignalAbort = () => {
        try {
          xhr.abort();
        } catch {
          /* ignore */
        }
      };
      signal.addEventListener('abort', onSignalAbort);
    }
    const detachSignal = (): void => {
      if (signal && onSignalAbort) {
        signal.removeEventListener('abort', onSignalAbort);
        onSignalAbort = null;
      }
    };

    if (onProgress) {
      xhr.upload.addEventListener('progress', (e) => {
        if (e.lengthComputable) onProgress(Math.round((e.loaded / e.total) * 100));
      });
    }

    xhr.addEventListener('load', () => {
      detachSignal();
      if (xhr.status === 413) {
        reject(new Error('FILE_TOO_LARGE'));
        return;
      }
      if (xhr.status < 200 || xhr.status >= 300) {
        reject(new Error(`Upload failed: ${xhr.status} ${xhr.statusText}`));
        return;
      }
      try {
        const parsed = JSON.parse(xhr.responseText) as unknown;
        // Unwrap the shared `{ success, data }` envelope when present, mirroring
        // `httpRequest`; otherwise treat the body itself as the asset.
        const asset =
          parsed && typeof parsed === 'object' && 'data' in parsed
            ? (parsed as { data: WorkshopAsset }).data
            : (parsed as WorkshopAsset);
        if (!asset || typeof asset !== 'object' || typeof (asset as WorkshopAsset).id !== 'string') {
          reject(new Error('Upload failed: server returned an unexpected response'));
        } else {
          resolve(normalizeAsset(asset));
        }
      } catch {
        reject(new Error('Upload failed: invalid server response'));
      }
    });

    xhr.addEventListener('error', () => {
      detachSignal();
      reject(new Error('Upload failed: network error'));
    });

    xhr.addEventListener('abort', () => {
      detachSignal();
      reject(new Error(WORKSHOP_UPLOAD_ABORTED));
    });

    xhr.send(formData);
  });
}

// ─────────────────────────────────────────────────────────────────────────────
// §3.3 Creation tasks
// ─────────────────────────────────────────────────────────────────────────────

/** Submit a generation task. */
export async function createTask(body: CreateTaskBody): Promise<CreationTask> {
  return normalizeCreationTask(await httpRequest<CreationTask>('POST', '/api/creation/tasks', body));
}

/** List generation tasks, optionally scoped to a canvas / status. */
export async function listTasks(query: ListTasksQuery = {}): Promise<CreationTask[]> {
  const qs = queryString({ canvas_id: query.canvas_id, status: query.status, limit: query.limit });
  const res = await httpRequest<{ tasks: CreationTask[] }>('GET', `/api/creation/tasks${qs}`);
  return (res?.tasks ?? []).map(normalizeCreationTask);
}

/** Fetch a single generation task. */
export async function getTask(id: CreationTaskId): Promise<CreationTask> {
  return normalizeCreationTask(
    await httpRequest<CreationTask>('GET', `/api/creation/tasks/${encodeURIComponent(id)}`)
  );
}

/** Cancel a generation task. */
export async function cancelTask(id: CreationTaskId): Promise<CreationTask> {
  return normalizeCreationTask(
    await httpRequest<CreationTask>('POST', `/api/creation/tasks/${encodeURIComponent(id)}/cancel`)
  );
}
