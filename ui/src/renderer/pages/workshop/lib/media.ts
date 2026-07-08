/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Workshop media loader.
 *
 * `GET /api/workshop/files/{id}` sits behind the auth gateway, so a bare
 * `<img src>` cannot reach it on every host (WebUI needs the CSRF/trust
 * headers). All workshop surfaces therefore load binaries through this
 * module: authenticated fetch → Blob → object URL, memoised per
 * `assetId(+thumb)` for the lifetime of the session.
 *
 * Frozen at M0 (append-only): downstream modules may add helpers but must not
 * change existing signatures.
 */

import { buildBackendAuthHeaders } from '@/common/adapter/httpBridge';
import { workshopFileUrl } from '../api';

const objectUrls = new Map<string, string>();
const inflight = new Map<string, Promise<string>>();

function cacheKey(assetId: string, thumb: boolean): string {
  return thumb ? `${assetId}#thumb` : assetId;
}

/**
 * Resolve an asset's binary to an object URL usable in `<img>` / `<video>`.
 * Concurrent callers share one request; results are cached until
 * {@link revokeAllWorkshopMedia} (or {@link revokeWorkshopMedia}) is called.
 */
export async function loadWorkshopMedia(assetId: string, opts: { thumb?: boolean } = {}): Promise<string> {
  const thumb = opts.thumb === true;
  const key = cacheKey(assetId, thumb);
  const cached = objectUrls.get(key);
  if (cached) return cached;
  const pending = inflight.get(key);
  if (pending) return pending;

  const task = (async () => {
    const res = await fetch(workshopFileUrl(assetId, thumb), {
      method: 'GET',
      headers: buildBackendAuthHeaders('GET'),
    });
    if (!res.ok) {
      throw new Error(`Workshop media load failed: ${res.status} ${res.statusText}`);
    }
    const blob = await res.blob();
    const url = URL.createObjectURL(blob);
    objectUrls.set(key, url);
    return url;
  })();

  inflight.set(key, task);
  try {
    return await task;
  } finally {
    inflight.delete(key);
  }
}

/** Drop one asset's cached object URLs (e.g. after replacing its binary). */
export function revokeWorkshopMedia(assetId: string): void {
  for (const key of [cacheKey(assetId, false), cacheKey(assetId, true)]) {
    const url = objectUrls.get(key);
    if (url) {
      URL.revokeObjectURL(url);
      objectUrls.delete(key);
    }
  }
}

/** Release every cached object URL (call when leaving the workshop surfaces). */
export function revokeAllWorkshopMedia(): void {
  for (const url of objectUrls.values()) URL.revokeObjectURL(url);
  objectUrls.clear();
}
