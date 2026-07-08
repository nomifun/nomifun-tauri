/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Drag-and-drop contract between the asset library panel (M4) and the canvas
 * (M1): dragging an asset out of the panel carries this payload; the canvas
 * `onDrop` turns it into an image/video/text node at the drop point.
 *
 * Frozen at M0 (append-only).
 */

import type { WorkshopAssetKind } from '../types';

/** DataTransfer MIME type for workshop asset drags. */
export const WORKSHOP_ASSET_DND = 'application/x-nomifun-workshop-asset';

export interface WorkshopAssetDragPayload {
  asset_id: string;
  kind: WorkshopAssetKind;
  title: string;
  width?: number | null;
  height?: number | null;
}

export function writeAssetDrag(dt: DataTransfer, payload: WorkshopAssetDragPayload): void {
  dt.setData(WORKSHOP_ASSET_DND, JSON.stringify(payload));
  dt.effectAllowed = 'copy';
}

export function readAssetDrag(dt: DataTransfer): WorkshopAssetDragPayload | null {
  const raw = dt.getData(WORKSHOP_ASSET_DND);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as WorkshopAssetDragPayload;
    return typeof parsed?.asset_id === 'string' ? parsed : null;
  } catch {
    return null;
  }
}
