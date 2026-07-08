/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Canvas node context — the handler surface node components call into.
 *
 * Keeping node `data` purely serializable (no callbacks) preserves react-flow
 * node-object identity across renders; the interactive behaviour lives here and
 * is consumed via {@link useCanvasNode}. The provider memoises a stable value so
 * updating canvas state never forces every node to re-render.
 */

import { createContext, useContext } from 'react';
import type { ImageEditorMode } from '../editor';
import type { ThemeMode } from './theme';

export interface CanvasNodeApi {
  theme: ThemeMode;
  /** Whether canvas interactions are enabled (false while panning/connecting). */
  interactive: boolean;
  /** Shallow-merge a patch into a node's `data` (records history + autosaves). */
  updateNodeData: (nodeId: string, patch: Record<string, unknown>) => void;
  /** Update a node's box size (used by aspect-lock toggle / fit-to-image). */
  resizeNode: (nodeId: string, size: { width: number; height: number }) => void;
  removeNode: (nodeId: string) => void;
  duplicateNode: (nodeId: string) => void;
  /** Upload a local file into an (empty) media node, replacing its asset. */
  fillNodeFromFile: (nodeId: string, file: File) => void;
  /** Open the big-image preview, seeded at this image node's asset. */
  previewImageNode: (nodeId: string) => void;
  /** Persist an asset into the library (in_library=1) with a toast. */
  saveAssetToLibrary: (assetId: string) => void;
  /** Download an asset to disk. */
  downloadAsset: (assetId: string, filename?: string) => void;
  /** Open the image editor for an image node in a specific mode. */
  editImageNode: (nodeId: string, mode: ImageEditorMode) => void;
  /**
   * Append-only (M8): open the big-image lightbox on an explicit asset list
   * (used by the output / compare nodes to preview a specific upstream result).
   */
  openImagePreview: (assetIds: string[], startIndex?: number) => void;
  /** Append-only (M8): dissolve a group, keeping its members (absolute coords restored). */
  ungroupNode: (groupId: string) => void;
  /** Append-only (M8): delete a group together with every member node. */
  deleteGroupWithChildren: (groupId: string) => void;
  /** Record a discrete history step + schedule a save (e.g. after resize end). */
  commitInteraction: () => void;
  beginInteraction: () => void;
}

const noop = (): void => {};

const DEFAULT_API: CanvasNodeApi = {
  theme: 'light',
  interactive: true,
  updateNodeData: noop,
  resizeNode: noop,
  removeNode: noop,
  duplicateNode: noop,
  fillNodeFromFile: noop,
  previewImageNode: noop,
  saveAssetToLibrary: noop,
  downloadAsset: noop,
  editImageNode: noop,
  openImagePreview: noop,
  ungroupNode: noop,
  deleteGroupWithChildren: noop,
  commitInteraction: noop,
  beginInteraction: noop,
};

export const CanvasNodeContext = createContext<CanvasNodeApi>(DEFAULT_API);

export function useCanvasNode(): CanvasNodeApi {
  return useContext(CanvasNodeContext);
}
