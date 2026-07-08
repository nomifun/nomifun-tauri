/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useEffect, useState } from 'react';

/** Loaded source image + resolved natural dimensions. */
export interface EditorImage {
  /** Decoded full-resolution element, reused for all exports. */
  el: HTMLImageElement;
  naturalWidth: number;
  naturalHeight: number;
}

export type EditorImageState =
  | { status: 'loading' }
  | { status: 'ready'; image: EditorImage }
  | { status: 'error'; error: string };

/**
 * Decode the source `src` (object URL / data URL, same-origin so the canvas
 * stays untainted) into a reusable {@link HTMLImageElement}. The natural size
 * from the request is trusted as a hint but always reconciled with the decoded
 * element's real dimensions.
 */
export function useEditorImage(src: string, hintW?: number, hintH?: number): EditorImageState {
  const [state, setState] = useState<EditorImageState>({ status: 'loading' });

  useEffect(() => {
    let cancelled = false;
    setState({ status: 'loading' });
    const el = new Image();
    el.decoding = 'async';
    el.src = src;
    el.decode()
      .then(() => {
        if (cancelled) return;
        const naturalWidth = el.naturalWidth || hintW || 0;
        const naturalHeight = el.naturalHeight || hintH || 0;
        if (naturalWidth < 1 || naturalHeight < 1) {
          setState({ status: 'error', error: 'invalid-dimensions' });
          return;
        }
        setState({ status: 'ready', image: { el, naturalWidth, naturalHeight } });
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        setState({ status: 'error', error: err instanceof Error ? err.message : String(err) });
      });
    return () => {
      cancelled = true;
    };
  }, [src, hintW, hintH]);

  return state;
}
