/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Pure run-pipeline for the generation card:
 *  - collect `@`-mention candidates from the canvas node set (auto-numbered)
 *  - resolve `data.mentions` refs to reference assets / text fragments
 *  - gather upstream (edge-connected) inputs
 *  - derive the {@link MediaCapability}, assemble the prompt, and build the
 *    ordered `inputs[]` for `POST /api/creation/tasks`
 *
 * Reference numbering (图1 / 图2 …) follows collection order — upstream edges
 * first (edge order), then mentions (mention order) — so the "图N" tokens a user
 * types line up with the inputs the backend receives, best-effort.
 */

import { buildBackendAuthHeaders } from '@/common/adapter/httpBridge';
import { workshopFileUrl } from '../api';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import type { CreationInput, MediaCapability, WorkshopAssetKind } from '../types';
import type { GenMode, MentionCandidate, ResolvedMention } from './genTypes';

// ─── Mention ref encoding ──────────────────────────────────────────────────────

export function mentionRefForNode(nodeId: string): string {
  return `node:${nodeId}`;
}

export function mentionRefForAsset(kind: WorkshopAssetKind, assetId: string): string {
  return `asset:${kind}:${assetId}`;
}

export interface ParsedMention {
  source: 'node' | 'asset';
  id: string;
  kind?: WorkshopAssetKind;
}

export function parseMentionRef(ref: string): ParsedMention | null {
  if (ref.startsWith('node:')) return { source: 'node', id: ref.slice(5) };
  if (ref.startsWith('asset:')) {
    const rest = ref.slice(6);
    const sep = rest.indexOf(':');
    if (sep < 0) return null;
    const kind = rest.slice(0, sep) as WorkshopAssetKind;
    return { source: 'asset', id: rest.slice(sep + 1), kind };
  }
  return null;
}

// ─── Node contributions ────────────────────────────────────────────────────────

interface NodeContribution {
  assetId: string | null;
  kind: WorkshopAssetKind;
  /** Inline text (text nodes); null when it must be fetched from `assetId`. */
  text: string | null;
}

/** What a flow node contributes as a generation input, or null if nothing yet. */
export function nodeContribution(node: WorkshopFlowNode): NodeContribution | null {
  const data = node.data as Record<string, unknown>;
  if (node.type === 'image') {
    const assetId = typeof data.assetId === 'string' ? data.assetId : null;
    return assetId ? { assetId, kind: 'image', text: null } : null;
  }
  if (node.type === 'video') {
    const assetId = typeof data.assetId === 'string' ? data.assetId : null;
    return assetId ? { assetId, kind: 'video', text: null } : null;
  }
  if (node.type === 'text') {
    const content = typeof data.content === 'string' ? data.content.trim() : '';
    return content ? { assetId: null, kind: 'text', text: content } : null;
  }
  if (node.type === 'generator') {
    const results = Array.isArray(data.resultAssetIds) ? (data.resultAssetIds as string[]) : [];
    if (!results.length) return null;
    const primary =
      (data.batch as { primary?: string } | undefined)?.primary && results.includes((data.batch as { primary?: string }).primary as string)
        ? (data.batch as { primary?: string }).primary!
        : results[0];
    const mode = typeof data.mode === 'string' ? (data.mode as GenMode) : 'image';
    const kind: WorkshopAssetKind = mode === 'video' ? 'video' : mode === 'text' ? 'text' : 'image';
    return { assetId: primary, kind, text: null };
  }
  return null;
}

// ─── Mention candidate collection (for the picker) ──────────────────────────────

const KIND_PREFIX: Record<WorkshopAssetKind, string> = { image: '图', video: '视频', text: '文' };

/**
 * Build the canvas-node mention candidates, auto-numbered per kind. Sorted by
 * position (top-to-bottom, left-to-right) so 图1 is the top-left-most image. The
 * card itself is excluded (a card can't reference its own pending output).
 */
export function collectNodeCandidates(nodes: WorkshopFlowNode[], selfId: string): MentionCandidate[] {
  const usable = nodes
    .filter((n) => n.id !== selfId && nodeContribution(n) !== null)
    .sort((a, b) => a.position.y - b.position.y || a.position.x - b.position.x);

  const counters: Record<WorkshopAssetKind, number> = { image: 0, video: 0, text: 0 };
  return usable.map((n) => {
    const contrib = nodeContribution(n)!;
    counters[contrib.kind] += 1;
    const label = `${KIND_PREFIX[contrib.kind]}${counters[contrib.kind]}`;
    return { ref: mentionRefForNode(n.id), label, kind: contrib.kind, source: 'node' };
  });
}

// ─── Text asset loading ─────────────────────────────────────────────────────────

/** Fetch a text asset's body through the auth gateway (best-effort). */
export async function loadWorkshopText(assetId: string): Promise<string | null> {
  try {
    const res = await fetch(workshopFileUrl(assetId), { method: 'GET', headers: buildBackendAuthHeaders('GET') });
    if (!res.ok) return null;
    return await res.text();
  } catch {
    return null;
  }
}

// ─── Run-plan assembly ──────────────────────────────────────────────────────────

export interface RunPlanInput {
  node: WorkshopFlowNode;
  nodes: WorkshopFlowNode[];
  edges: WorkshopFlowEdge[];
  mode: GenMode;
  mentions: string[];
  maskAssetId?: string;
  basePrompt: string;
  /**
   * Append-only (M8, loop node): keep only the image references falling in the
   * half-open window `[offset, offset+size)` of the collected image sequence.
   * Non-image references (e.g. video) are unaffected. Absent ⇒ all images kept.
   */
  imageWindow?: { offset: number; size: number };
  /**
   * Append-only (M8, loop node): a line prepended to the assembled prompt (the
   * rendered count-injection template). Absent ⇒ no prefix.
   */
  promptPrefix?: string;
}

export interface RunPlan {
  capability: MediaCapability;
  inputs: CreationInput[];
  prompt: string;
  /** Count of image/video reference inputs (drives 图N legend). */
  referenceCount: number;
}

/** Resolve a mention ref to a reference asset id / text fragment (async for text). */
async function resolveMention(ref: string, nodes: WorkshopFlowNode[]): Promise<ResolvedMention | null> {
  const parsed = parseMentionRef(ref);
  if (!parsed) return null;
  if (parsed.source === 'node') {
    const node = nodes.find((n) => n.id === parsed.id);
    if (!node) return null;
    const contrib = nodeContribution(node);
    if (!contrib) return null;
    if (contrib.kind === 'text') {
      const text = contrib.text ?? (contrib.assetId ? await loadWorkshopText(contrib.assetId) : null);
      return { ref, label: ref, kind: 'text', assetId: null, text };
    }
    return { ref, label: ref, kind: contrib.kind, assetId: contrib.assetId, text: null };
  }
  // Library asset.
  const kind = parsed.kind ?? 'image';
  if (kind === 'text') {
    return { ref, label: ref, kind: 'text', assetId: null, text: await loadWorkshopText(parsed.id) };
  }
  return { ref, label: ref, kind, assetId: parsed.id, text: null };
}

/**
 * Assemble everything the backend needs. Async because text nodes / text assets
 * may need their body fetched. Reference assets are de-duplicated by id while
 * preserving first-seen order.
 */
export async function buildRunPlan(input: RunPlanInput): Promise<RunPlan> {
  const { node, nodes, edges, mode, mentions, maskAssetId, basePrompt, imageWindow, promptPrefix } = input;

  const refAssets: { assetId: string; kind: WorkshopAssetKind }[] = [];
  const texts: string[] = [];
  const seen = new Set<string>();

  const pushRef = (assetId: string | null, kind: WorkshopAssetKind): void => {
    if (!assetId || seen.has(assetId)) return;
    seen.add(assetId);
    refAssets.push({ assetId, kind });
  };

  // 1) Upstream edge-connected nodes (in edge order). A group source (M8) expands
  //    into its members, ordered by position, so a group acts as an input group.
  const upstreamSources: WorkshopFlowNode[] = [];
  for (const edge of edges) {
    if (edge.target !== node.id) continue;
    const src = nodes.find((n) => n.id === edge.source);
    if (!src) continue;
    if (src.type === 'group') {
      const members = nodes
        .filter((n) => n.parentId === src.id)
        .sort((a, b) => a.position.y - b.position.y || a.position.x - b.position.x);
      upstreamSources.push(...members);
    } else {
      upstreamSources.push(src);
    }
  }
  for (const src of upstreamSources) {
    const contrib = nodeContribution(src);
    if (!contrib) continue;
    if (contrib.kind === 'text') {
      const text = contrib.text ?? (contrib.assetId ? await loadWorkshopText(contrib.assetId) : null);
      if (text) texts.push(text);
    } else {
      pushRef(contrib.assetId, contrib.kind);
    }
  }

  // 2) Mentions (in mention order).
  for (const ref of mentions) {
    const resolved = await resolveMention(ref, nodes);
    if (!resolved) continue;
    if (resolved.kind === 'text') {
      if (resolved.text) texts.push(resolved.text);
    } else {
      pushRef(resolved.assetId, resolved.kind);
    }
  }

  // 2b) Loop windowing: keep only the image references inside the window.
  let effectiveRefs = refAssets;
  if (imageWindow) {
    const start = Math.max(0, Math.round(imageWindow.offset));
    const end = start + Math.max(0, Math.round(imageWindow.size));
    const windowed = new Set(
      refAssets
        .filter((r) => r.kind === 'image')
        .slice(start, end)
        .map((r) => r.assetId)
    );
    effectiveRefs = refAssets.filter((r) => r.kind !== 'image' || windowed.has(r.assetId));
  }

  // 3) Build inputs.
  const inputs: CreationInput[] = [];
  let imageRefCount = 0;
  for (const ref of effectiveRefs) {
    if (ref.kind === 'image') {
      // Image references drive i2i / i2v and get numbered 图N.
      inputs.push({ asset_id: ref.assetId, role: 'reference' });
      imageRefCount += 1;
    } else if (mode === 'video' && ref.kind === 'video') {
      inputs.push({ asset_id: ref.assetId, role: 'video' });
    }
    // image-mode video refs are ignored (no capability path).
  }
  if (mode === 'image' && maskAssetId) {
    inputs.push({ asset_id: maskAssetId, role: 'mask' });
  }

  // 4) Capability.
  let capability: MediaCapability;
  if (mode === 'text') {
    capability = 'text';
  } else if (mode === 'video') {
    capability = imageRefCount > 0 ? 'i2v' : 't2v';
  } else if (maskAssetId) {
    capability = 'inpaint';
  } else {
    capability = imageRefCount > 0 ? 'i2i' : 't2i';
  }

  // 5) Prompt (optional count-injection prefix + base + upstream/mention text).
  const prompt = [promptPrefix?.trim() ?? '', basePrompt.trim(), ...texts]
    .filter((s) => s.length > 0)
    .join('\n\n');

  return { capability, inputs, prompt, referenceCount: effectiveRefs.length };
}
