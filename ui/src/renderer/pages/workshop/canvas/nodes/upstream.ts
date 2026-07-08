/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Resolve the "primary asset" an upstream node contributes for display purposes
 * (compare / output nodes). Mirrors the generation pipeline's `nodeContribution`
 * but works off the lean `{ type, data }` shape that `useNodesData` returns, so
 * node components can read a source's live result without a full flow node.
 */

import type { WorkshopAssetKind, WorkshopGeneratorMode } from '../../types';

export interface UpstreamPrimary {
  assetId: string | null;
  kind: WorkshopAssetKind;
  /** Inline text for text nodes; null otherwise. */
  text: string | null;
}

export function upstreamPrimary(node: { type?: string; data?: unknown } | null | undefined): UpstreamPrimary | null {
  if (!node || !node.type) return null;
  const data = (node.data ?? {}) as Record<string, unknown>;
  if (node.type === 'image') {
    const assetId = typeof data.assetId === 'string' ? data.assetId : null;
    return assetId ? { assetId, kind: 'image', text: null } : null;
  }
  if (node.type === 'video') {
    const assetId = typeof data.assetId === 'string' ? data.assetId : null;
    return assetId ? { assetId, kind: 'video', text: null } : null;
  }
  if (node.type === 'text') {
    const content = typeof data.content === 'string' ? data.content : '';
    return content.trim() ? { assetId: null, kind: 'text', text: content } : null;
  }
  if (node.type === 'generator') {
    const results = Array.isArray(data.resultAssetIds) ? (data.resultAssetIds as string[]) : [];
    if (!results.length) return null;
    const batch = data.batch as { primary?: string } | undefined;
    const primary = batch?.primary && results.includes(batch.primary) ? batch.primary : results[0];
    const mode = typeof data.mode === 'string' ? (data.mode as WorkshopGeneratorMode) : 'image';
    const kind: WorkshopAssetKind = mode === 'video' ? 'video' : mode === 'text' ? 'text' : 'image';
    return { assetId: primary, kind, text: null };
  }
  return null;
}
