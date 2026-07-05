/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Media helpers for the canvas: a React hook that resolves a workshop asset to
 * a displayable object URL (through the frozen `loadWorkshopMedia` contract),
 * plus small utilities for reading image dimensions and picking local files.
 */

import { useEffect, useRef, useState } from 'react';
import { loadWorkshopMedia } from '../lib/media';

export type MediaState =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ready'; url: string }
  | { status: 'error'; message: string };

/**
 * Resolve an asset id to an object URL. Re-runs when `assetId` or `nonce`
 * changes (bump `nonce` after replacing an asset's binary to bust the cache).
 */
export function useWorkshopMedia(assetId: string | null | undefined, nonce = 0): MediaState {
  const [state, setState] = useState<MediaState>({ status: assetId ? 'loading' : 'idle' });
  const reqRef = useRef(0);

  useEffect(() => {
    if (!assetId) {
      setState({ status: 'idle' });
      return;
    }
    const req = reqRef.current + 1;
    reqRef.current = req;
    setState({ status: 'loading' });
    let cancelled = false;
    loadWorkshopMedia(assetId)
      .then((url) => {
        if (cancelled || reqRef.current !== req) return;
        setState({ status: 'ready', url });
      })
      .catch((e: unknown) => {
        if (cancelled || reqRef.current !== req) return;
        setState({ status: 'error', message: e instanceof Error ? e.message : String(e) });
      });
    return () => {
      cancelled = true;
    };
  }, [assetId, nonce]);

  return state;
}

/** Read an image file's natural dimensions (best-effort; resolves null on failure). */
export function readImageSize(file: File | Blob): Promise<{ width: number; height: number } | null> {
  return new Promise((resolve) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      const size = { width: img.naturalWidth, height: img.naturalHeight };
      URL.revokeObjectURL(url);
      resolve(size.width > 0 && size.height > 0 ? size : null);
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      resolve(null);
    };
    img.src = url;
  });
}

export function isImageFile(file: { type?: string; name?: string }): boolean {
  if (file.type) return file.type.startsWith('image/');
  return /\.(png|jpe?g|gif|webp|bmp|svg|avif)$/i.test(file.name ?? '');
}

export function isVideoFile(file: { type?: string; name?: string }): boolean {
  if (file.type) return file.type.startsWith('video/');
  return /\.(mp4|webm|mov|mkv|avi|m4v)$/i.test(file.name ?? '');
}

/** Open a native file picker and resolve the chosen files (empty if cancelled). */
export function pickFiles(accept: string, multiple = false): Promise<File[]> {
  return new Promise((resolve) => {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = accept;
    input.multiple = multiple;
    input.style.position = 'fixed';
    input.style.left = '-9999px';
    let settled = false;
    const done = (files: File[]): void => {
      if (settled) return;
      settled = true;
      window.removeEventListener('focus', onFocus, true);
      input.remove();
      resolve(files);
    };
    input.addEventListener('change', () => done(input.files ? Array.from(input.files) : []));
    // Fallback: if the dialog is dismissed, `change` never fires — resolve empty
    // shortly after the window regains focus.
    const onFocus = (): void => {
      window.setTimeout(() => done([]), 400);
    };
    window.addEventListener('focus', onFocus, true);
    document.body.appendChild(input);
    input.click();
  });
}
