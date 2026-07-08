/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * React binding over `loadWorkshopMedia` (M0 `lib/media.ts`): resolves an
 * asset's binary to a memoised object URL for use in `<img>` / `<video>`.
 * Every workshop surface loads binaries through the gated `/files/{id}`
 * endpoint via this path — a bare `<img src>` cannot carry the auth headers.
 */

import { useEffect, useState } from 'react';

import { loadWorkshopMedia } from '../lib/media';

export type WorkshopMediaStatus = 'idle' | 'loading' | 'ready' | 'error';

export interface WorkshopMediaState {
  url: string | null;
  status: WorkshopMediaStatus;
}

/**
 * Load an asset's object URL. Pass `thumb` for the small preview and `enabled`
 * to defer the fetch (e.g. until a card scrolls into view or a modal opens).
 * Re-fetches when the asset id / thumb flag changes; the underlying cache
 * dedupes concurrent and repeat requests.
 */
export function useWorkshopObjectUrl(
  assetId: string | null | undefined,
  opts: { thumb?: boolean; enabled?: boolean } = {}
): WorkshopMediaState {
  const { thumb = false, enabled = true } = opts;
  const [state, setState] = useState<WorkshopMediaState>({ url: null, status: 'idle' });

  useEffect(() => {
    if (!assetId || !enabled) {
      setState({ url: null, status: 'idle' });
      return;
    }
    let cancelled = false;
    setState({ url: null, status: 'loading' });
    loadWorkshopMedia(assetId, { thumb })
      .then((url) => {
        if (!cancelled) setState({ url, status: 'ready' });
      })
      .catch(() => {
        if (!cancelled) setState({ url: null, status: 'error' });
      });
    return () => {
      cancelled = true;
    };
  }, [assetId, thumb, enabled]);

  return state;
}
