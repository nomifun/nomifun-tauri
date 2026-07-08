/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Image editor entry point (M5 module).
 *
 * The canvas (M1/M7) opens the editor for an image node via
 * {@link openImageEditor}; the returned result is uploaded as new assets /
 * nodes by the caller. The types below are the frozen M0 contract — their
 * shapes must not change. {@link openImageEditor} is implemented as a
 * command-imperative full-screen overlay: it mounts a fresh React root on a
 * detached container, and resolves + unmounts when the user applies or cancels.
 */

import { createRoot } from 'react-dom/client';
import React from 'react';
import ImageEditorModal from './ImageEditorModal';

export type ImageEditorMode = 'crop' | 'mask' | 'split' | 'upscale';

export interface ImageEditorRequest {
  mode: ImageEditorMode;
  /** Object URL / data URL of the source image (caller resolves via lib/media). */
  src: string;
  naturalWidth?: number;
  naturalHeight?: number;
}

export type ImageEditorResult =
  | { type: 'crop'; blob: Blob }
  /** Painted area is transparent (alpha 0) on an otherwise opaque copy. */
  | { type: 'mask'; maskBlob: Blob; prompt: string }
  | { type: 'split'; pieces: { blob: Blob; row: number; col: number }[] }
  | { type: 'upscale'; blob: Blob };

/**
 * Open the modal image editor. Resolves with the edit result, or `null` when
 * the user cancels (Esc, the close button, or Cancel).
 */
export async function openImageEditor(req: ImageEditorRequest): Promise<ImageEditorResult | null> {
  return new Promise<ImageEditorResult | null>((resolve) => {
    const host = document.createElement('div');
    host.setAttribute('data-workshop-image-editor', '');
    document.body.appendChild(host);
    const root = createRoot(host);

    let settled = false;
    const close = (result: ImageEditorResult | null) => {
      if (settled) return;
      settled = true;
      // Defer unmount so we never unmount synchronously from inside a React
      // event/render pass of the root we're tearing down.
      setTimeout(() => {
        root.unmount();
        host.remove();
      }, 0);
      resolve(result);
    };

    root.render(React.createElement(ImageEditorModal, { req, onClose: close }));
  });
}
