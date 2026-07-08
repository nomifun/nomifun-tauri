/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Canvas model — the pure (React-free) glue between the frontend-owned canvas
 * doc (`WorkshopCanvasDoc`, contract §4) and `@xyflow/react`'s native
 * node/edge structures.
 *
 * The canvas works internally in react-flow's shape; this module converts to /
 * from the doc on load / save, builds content-only history snapshots, and mints
 * fresh nodes. Node/edge ids are frontend-owned opaque strings (the backend
 * never parses the doc), so we mint short uuid-ish ids here.
 */

import type { Edge, Node } from '@xyflow/react';
import type {
  WorkshopCanvasBackground,
  WorkshopCanvasDoc,
  WorkshopCompareNodeData,
  WorkshopGeneratorMode,
  WorkshopGeneratorNodeData,
  WorkshopGroupNodeData,
  WorkshopImageNodeData,
  WorkshopLoopMode,
  WorkshopLoopNodeData,
  WorkshopNode,
  WorkshopNodeKind,
  WorkshopOutputNodeData,
  WorkshopTextNodeData,
  WorkshopVideoNodeData,
  WorkshopViewport,
} from '../types';
import { WORKSHOP_DOC_SCHEMA } from '../types';

// ─────────────────────────────────────────────────────────────────────────────
// Flow node/edge types
// ─────────────────────────────────────────────────────────────────────────────

/**
 * react-flow requires a node's `data` to satisfy `Record<string, unknown>`.
 * The doc data interfaces don't carry an index signature, so we intersect them
 * with `Record<string, unknown>` for the flow layer — the on-disk shape is
 * unchanged (the extra index signature is a compile-time-only widening).
 */
export type ImageNodeData = WorkshopImageNodeData & Record<string, unknown>;
export type TextNodeData = WorkshopTextNodeData & Record<string, unknown>;
export type VideoNodeData = WorkshopVideoNodeData & Record<string, unknown>;
export type GeneratorNodeData = WorkshopGeneratorNodeData & Record<string, unknown>;
export type LoopNodeData = WorkshopLoopNodeData & Record<string, unknown>;
export type CompareNodeData = WorkshopCompareNodeData & Record<string, unknown>;
export type OutputNodeData = WorkshopOutputNodeData & Record<string, unknown>;
export type GroupNodeData = WorkshopGroupNodeData & Record<string, unknown>;

export type ImageFlowNode = Node<ImageNodeData, 'image'>;
export type TextFlowNode = Node<TextNodeData, 'text'>;
export type VideoFlowNode = Node<VideoNodeData, 'video'>;
export type GeneratorFlowNode = Node<GeneratorNodeData, 'generator'>;
export type LoopFlowNode = Node<LoopNodeData, 'loop'>;
export type CompareFlowNode = Node<CompareNodeData, 'compare'>;
export type OutputFlowNode = Node<OutputNodeData, 'output'>;
export type GroupFlowNode = Node<GroupNodeData, 'group'>;

/** The M8 flow-node kinds (loop / compare / output / group). */
export type PlaceholderFlowNode = LoopFlowNode | CompareFlowNode | OutputFlowNode | GroupFlowNode;

/** Any node the workshop canvas can render. */
export type WorkshopFlowNode =
  | ImageFlowNode
  | TextFlowNode
  | VideoFlowNode
  | GeneratorFlowNode
  | LoopFlowNode
  | CompareFlowNode
  | OutputFlowNode
  | GroupFlowNode;

export type WorkshopFlowEdge = Edge;

// ─────────────────────────────────────────────────────────────────────────────
// Per-kind metadata (default sizes, minimap tint)
// ─────────────────────────────────────────────────────────────────────────────

export interface KindMeta {
  defaultWidth: number;
  defaultHeight: number;
  minWidth: number;
  minHeight: number;
  /** Minimap literal fill (react-flow's minimap can't resolve CSS vars). */
  minimap: { light: string; dark: string };
}

export const KIND_META: Record<WorkshopNodeKind, KindMeta> = {
  image: {
    defaultWidth: 240,
    defaultHeight: 200,
    minWidth: 96,
    minHeight: 72,
    minimap: { light: '#2f6bff', dark: '#5b8bff' },
  },
  text: {
    defaultWidth: 240,
    defaultHeight: 132,
    minWidth: 140,
    minHeight: 64,
    minimap: { light: '#d97706', dark: '#f59e0b' },
  },
  video: {
    defaultWidth: 300,
    defaultHeight: 196,
    minWidth: 160,
    minHeight: 110,
    minimap: { light: '#7c3aed', dark: '#a78bfa' },
  },
  generator: {
    defaultWidth: 300,
    defaultHeight: 220,
    minWidth: 240,
    minHeight: 160,
    minimap: { light: '#16a34a', dark: '#22c55e' },
  },
  loop: {
    defaultWidth: 240,
    defaultHeight: 148,
    minWidth: 180,
    minHeight: 120,
    minimap: { light: '#0891b2', dark: '#22d3ee' },
  },
  compare: {
    defaultWidth: 300,
    defaultHeight: 200,
    minWidth: 200,
    minHeight: 140,
    minimap: { light: '#db2777', dark: '#f472b6' },
  },
  output: {
    defaultWidth: 240,
    defaultHeight: 160,
    minWidth: 180,
    minHeight: 120,
    minimap: { light: '#64748b', dark: '#94a3b8' },
  },
  group: {
    defaultWidth: 320,
    defaultHeight: 220,
    minWidth: 200,
    minHeight: 160,
    minimap: { light: '#94a3b8', dark: '#64748b' },
  },
};

/** Placeholder kinds not yet interactive (M8). */
export const PLACEHOLDER_KINDS: WorkshopNodeKind[] = ['loop', 'compare', 'output', 'group'];

/** Padding a group node wraps around its members (title-bar top, uniform sides/bottom). */
export const GROUP_PADDING = { top: 40, side: 18, bottom: 18 } as const;

/** Viewport zoom bounds (mouse-anchored wheel zoom stays within these). */
export const ZOOM_MIN = 0.05;
export const ZOOM_MAX = 4;

/** Shared fitView tuning (initial mount + ResizeObserver refit + fit button). */
export const FIT_VIEW_OPTIONS = { padding: 0.2, maxZoom: 1.5, duration: 240 } as const;

/** Offset (px) applied to pasted / duplicated nodes so clones don't overlap. */
export const PASTE_OFFSET = 24;

// ─────────────────────────────────────────────────────────────────────────────
// Id minting (frontend-owned opaque ids)
// ─────────────────────────────────────────────────────────────────────────────

function randomToken(): string {
  try {
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
      return crypto.randomUUID().replace(/-/g, '').slice(0, 20);
    }
  } catch {
    /* fall through */
  }
  return `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
}

export function newNodeId(): string {
  return `wsn_${randomToken()}`;
}

export function newEdgeId(): string {
  return `wse_${randomToken()}`;
}

// ─────────────────────────────────────────────────────────────────────────────
// Doc ⇄ flow conversion
// ─────────────────────────────────────────────────────────────────────────────

function sizeFor(node: WorkshopNode): { width: number; height: number } {
  const meta = KIND_META[node.kind];
  const width = Number.isFinite(node.w) && node.w > 0 ? node.w : meta.defaultWidth;
  const height = Number.isFinite(node.h) && node.h > 0 ? node.h : meta.defaultHeight;
  return { width, height };
}

/**
 * Reorder nodes so every group parent precedes its children — react-flow requires
 * a parent to appear before any node that references it via `parentId`. Root order
 * is preserved; each parent's children are grouped immediately after it.
 */
export function orderNodesParentFirst(nodes: WorkshopFlowNode[]): WorkshopFlowNode[] {
  if (!nodes.some((n) => n.parentId)) return nodes;
  const ids = new Set(nodes.map((n) => n.id));
  const byParent = new Map<string, WorkshopFlowNode[]>();
  const roots: WorkshopFlowNode[] = [];
  for (const n of nodes) {
    if (n.parentId && ids.has(n.parentId)) {
      const arr = byParent.get(n.parentId) ?? [];
      arr.push(n);
      byParent.set(n.parentId, arr);
    } else {
      roots.push(n);
    }
  }
  const out: WorkshopFlowNode[] = [];
  for (const r of roots) {
    out.push(r);
    const kids = byParent.get(r.id);
    if (kids) out.push(...kids);
  }
  return out;
}

/** Convert a persisted doc into react-flow nodes + edges. */
export function docToFlow(doc: WorkshopCanvasDoc): { nodes: WorkshopFlowNode[]; edges: WorkshopFlowEdge[] } {
  // Group positions (absolute) let us re-derive members' parent-relative coords.
  const groupPos = new Map<string, { x: number; y: number }>();
  for (const n of doc.nodes) if (n.kind === 'group') groupPos.set(n.id, { x: n.x, y: n.y });

  const nodes: WorkshopFlowNode[] = doc.nodes.map((n) => {
    const { width, height } = sizeFor(n);
    const parentAbs = n.groupId ? groupPos.get(n.groupId) : undefined;
    const flow = {
      id: n.id,
      type: n.kind,
      position: parentAbs ? { x: n.x - parentAbs.x, y: n.y - parentAbs.y } : { x: n.x, y: n.y },
      width,
      height,
      data: { ...(n.data as Record<string, unknown>) },
    } as WorkshopFlowNode;
    if (parentAbs && n.groupId) {
      flow.parentId = n.groupId;
      flow.extent = 'parent';
    }
    // Groups are deleted via their own menu (ungroup / delete-with-children) so
    // the Delete key never orphans members.
    if (n.kind === 'group') flow.deletable = false;
    return flow;
  });

  const ordered = orderNodesParentFirst(nodes);
  const nodeIds = new Set(ordered.map((n) => n.id));
  const edges: WorkshopFlowEdge[] = doc.edges
    .filter((e) => nodeIds.has(e.from) && nodeIds.has(e.to))
    .map((e) => ({ id: e.id, source: e.from, target: e.to }));

  return { nodes: ordered, edges };
}

function nodeSize(node: WorkshopFlowNode): { width: number; height: number } {
  const meta = KIND_META[node.type as WorkshopNodeKind] ?? KIND_META.image;
  const width = node.width ?? node.measured?.width ?? meta.defaultWidth;
  const height = node.height ?? node.measured?.height ?? meta.defaultHeight;
  return { width: Math.round(width), height: Math.round(height) };
}

/** Rebuild a persistable doc from the live flow state. */
export function flowToDoc(
  nodes: WorkshopFlowNode[],
  edges: WorkshopFlowEdge[],
  viewport: WorkshopViewport,
  background: WorkshopCanvasBackground
): WorkshopCanvasDoc {
  // Parent positions (absolute) let us re-derive members' absolute canvas coords.
  const posById = new Map<string, { x: number; y: number }>();
  for (const n of nodes) posById.set(n.id, { x: n.position.x, y: n.position.y });

  return {
    schema: WORKSHOP_DOC_SCHEMA,
    viewport,
    background,
    nodes: nodes.map((n) => {
      const { width, height } = nodeSize(n);
      const parentPos = n.parentId ? posById.get(n.parentId) : undefined;
      const absX = parentPos ? n.position.x + parentPos.x : n.position.x;
      const absY = parentPos ? n.position.y + parentPos.y : n.position.y;
      return {
        id: n.id,
        kind: n.type as WorkshopNodeKind,
        x: Math.round(absX),
        y: Math.round(absY),
        w: width,
        h: height,
        groupId: n.parentId ?? null,
        data: { ...(n.data as Record<string, unknown>) },
      } as WorkshopNode;
    }),
    edges: edges.map((e) => ({ id: e.id, from: e.source, to: e.target })),
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// History snapshots (content-only — no selection / measured noise)
// ─────────────────────────────────────────────────────────────────────────────

export interface CanvasSnapshot {
  nodes: WorkshopFlowNode[];
  edges: WorkshopFlowEdge[];
  background: WorkshopCanvasBackground;
}

/**
 * Normalize a node's `data` for history so undo/redo can never resurrect a
 * generator's in-flight run state. A non-terminal generator status
 * (`queued` / `running`) is coerced to `idle` and its `taskId` cleared; terminal
 * states (`success` / `error`) and `resultAssetIds` are preserved verbatim.
 * Other node kinds pass through untouched.
 *
 * Only the in-memory history is scrubbed — the persisted doc (`flowToDoc`) keeps
 * the live run state on purpose, so a full canvas reload remounts the card and
 * resumes polling the (by-then-terminal) task instead of losing the result.
 */
function snapshotNodeData(node: WorkshopFlowNode): Record<string, unknown> {
  const data = { ...(node.data as Record<string, unknown>) };
  if (node.type === 'generator' && (data.status === 'queued' || data.status === 'running')) {
    data.status = 'idle';
    data.taskId = null;
  }
  return data;
}

/** Strip transient fields so measurement / selection churn never lands in history. */
export function buildSnapshot(
  nodes: WorkshopFlowNode[],
  edges: WorkshopFlowEdge[],
  background: WorkshopCanvasBackground
): CanvasSnapshot {
  return {
    background,
    nodes: nodes.map((n) => {
      const { width, height } = nodeSize(n);
      return {
        ...n,
        selected: false,
        dragging: false,
        measured: undefined,
        width,
        height,
        position: { x: Math.round(n.position.x), y: Math.round(n.position.y) },
        data: snapshotNodeData(n),
      };
    }) as WorkshopFlowNode[],
    edges: edges.map((e) => ({ id: e.id, source: e.source, target: e.target })),
  };
}

/** Cheap structural equality for snapshots (content-only, so stable). */
export function snapshotSignature(snap: CanvasSnapshot): string {
  return JSON.stringify(snap);
}

/** Rehydrate flow state from a snapshot (fresh object identities, unselected). */
export function snapshotToState(snap: CanvasSnapshot): {
  nodes: WorkshopFlowNode[];
  edges: WorkshopFlowEdge[];
  background: WorkshopCanvasBackground;
} {
  return {
    background: snap.background,
    nodes: snap.nodes.map((n) => ({
      ...n,
      selected: false,
      dragging: false,
      measured: undefined,
      data: snapshotNodeData(n),
    })) as WorkshopFlowNode[],
    edges: snap.edges.map((e) => ({ id: e.id, source: e.source, target: e.target })),
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// Node factories
// ─────────────────────────────────────────────────────────────────────────────

export interface XY {
  x: number;
  y: number;
}

function base(kind: WorkshopNodeKind, position: XY): { id: string; position: XY; width: number; height: number } {
  const meta = KIND_META[kind];
  return { id: newNodeId(), position, width: meta.defaultWidth, height: meta.defaultHeight };
}

/** Constrain an image node's initial box to a natural aspect ratio (capped). */
function imageBox(naturalWidth?: number, naturalHeight?: number): { width: number; height: number } {
  const meta = KIND_META.image;
  if (!naturalWidth || !naturalHeight || naturalWidth <= 0 || naturalHeight <= 0) {
    return { width: meta.defaultWidth, height: meta.defaultHeight };
  }
  const maxW = 320;
  const width = Math.min(naturalWidth, maxW);
  const height = Math.max(meta.minHeight, Math.round((width * naturalHeight) / naturalWidth));
  return { width, height };
}

export function makeImageNode(position: XY, data: Partial<ImageNodeData> = {}): ImageFlowNode {
  const box = imageBox(data.naturalWidth, data.naturalHeight);
  return {
    id: newNodeId(),
    type: 'image',
    position,
    width: box.width,
    height: box.height,
    data: { assetId: null, lockAspect: true, ...data },
  };
}

export function makeTextNode(position: XY, data: Partial<TextNodeData> = {}): TextFlowNode {
  const b = base('text', position);
  return {
    id: b.id,
    type: 'text',
    position: b.position,
    width: b.width,
    height: b.height,
    data: { content: '', fontSize: 14, ...data },
  };
}

export function makeVideoNode(position: XY, data: Partial<VideoNodeData> = {}): VideoFlowNode {
  const b = base('video', position);
  return {
    id: b.id,
    type: 'video',
    position: b.position,
    width: b.width,
    height: b.height,
    data: { assetId: null, ...data },
  };
}

export function makeGeneratorNode(
  position: XY,
  mode: WorkshopGeneratorMode = 'image',
  data: Partial<GeneratorNodeData> = {}
): GeneratorFlowNode {
  const b = base('generator', position);
  return {
    id: b.id,
    type: 'generator',
    position: b.position,
    width: b.width,
    height: b.height,
    data: {
      mode,
      prompt: '',
      params: {},
      mentions: [],
      status: 'idle',
      resultAssetIds: [],
      ...data,
    },
  };
}

export function makeLoopNode(position: XY, data: Partial<LoopNodeData> = {}): LoopFlowNode {
  const b = base('loop', position);
  return {
    id: b.id,
    type: 'loop',
    position: b.position,
    width: b.width,
    height: b.height,
    data: { count: 4, start: 1, batch: 1, loopMode: 'serial', countTemplate: '现在生成第 {i} 张', ...data },
  };
}

export function makeCompareNode(position: XY, data: Partial<CompareNodeData> = {}): CompareFlowNode {
  const b = base('compare', position);
  return {
    id: b.id,
    type: 'compare',
    position: b.position,
    width: b.width,
    height: b.height,
    data: { split: 0.5, ...data },
  };
}

export function makeOutputNode(position: XY, data: Partial<OutputNodeData> = {}): OutputFlowNode {
  const b = base('output', position);
  return {
    id: b.id,
    type: 'output',
    position: b.position,
    width: b.width,
    height: b.height,
    data: { ...data },
  };
}

/**
 * Mint a group container node. Groups render behind their members, are dragged
 * as a unit, and are non-deletable via the Delete key (removed through their own
 * ungroup / delete-with-children menu so members are never orphaned).
 */
export function makeGroupNode(
  position: XY,
  size: { width: number; height: number },
  data: Partial<GroupNodeData> = {}
): GroupFlowNode {
  return {
    id: newNodeId(),
    type: 'group',
    position,
    width: size.width,
    height: size.height,
    deletable: false,
    data: { title: '分组', ...data },
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// Grouping (Ctrl+G / Ctrl+Shift+G) — sub-flow parenting
// ─────────────────────────────────────────────────────────────────────────────

/** A node's absolute canvas bounds (accounting for parent-relative children). */
function absoluteRect(
  node: WorkshopFlowNode,
  parentPos: Map<string, { x: number; y: number }>
): { x: number; y: number; w: number; h: number } {
  const p = node.parentId ? parentPos.get(node.parentId) : undefined;
  const { width, height } = nodeSize(node);
  const x = (p ? p.x : 0) + node.position.x;
  const y = (p ? p.y : 0) + node.position.y;
  return { x, y, w: width, h: height };
}

/**
 * Group ≥2 free nodes: mints a group node sized to wrap the members (with a
 * title-bar top pad), reparents each member (`parentId` + `extent: 'parent'` +
 * relative position), and returns a fully re-ordered node array (group first).
 * Members already in a group, and group nodes themselves, are ignored (no
 * nesting). Returns `null` when fewer than two members qualify.
 */
export function groupSelectedNodes(
  nodes: WorkshopFlowNode[],
  selectedIds: string[],
  title: string
): { nodes: WorkshopFlowNode[]; groupId: string } | null {
  const selected = new Set(selectedIds);
  const members = nodes.filter((n) => selected.has(n.id) && !n.parentId && n.type !== 'group');
  if (members.length < 2) return null;

  const posById = new Map<string, { x: number; y: number }>();
  for (const n of nodes) posById.set(n.id, { x: n.position.x, y: n.position.y });

  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const m of members) {
    const r = absoluteRect(m, posById);
    minX = Math.min(minX, r.x);
    minY = Math.min(minY, r.y);
    maxX = Math.max(maxX, r.x + r.w);
    maxY = Math.max(maxY, r.y + r.h);
  }

  const gx = minX - GROUP_PADDING.side;
  const gy = minY - GROUP_PADDING.top;
  const gw = maxX - minX + GROUP_PADDING.side * 2;
  const gh = maxY - minY + GROUP_PADDING.top + GROUP_PADDING.bottom;
  const group = makeGroupNode({ x: gx, y: gy }, { width: gw, height: gh }, { title });

  const memberIds = new Set(members.map((m) => m.id));
  const reparented: WorkshopFlowNode[] = members.map((m) => {
    const r = absoluteRect(m, posById);
    return {
      ...m,
      parentId: group.id,
      extent: 'parent',
      selected: false,
      position: { x: r.x - gx, y: r.y - gy },
      data: { ...m.data },
    } as WorkshopFlowNode;
  });

  const others = nodes.filter((n) => !memberIds.has(n.id));
  return { nodes: orderNodesParentFirst([group, ...reparented, ...others]), groupId: group.id };
}

/**
 * Ungroup a group node: free every member (absolute position restored, parenting
 * cleared) and drop the group node. Returns the new node array, or `null` when
 * the id isn't a group.
 */
export function ungroupNodes(nodes: WorkshopFlowNode[], groupId: string): WorkshopFlowNode[] | null {
  const group = nodes.find((n) => n.id === groupId && n.type === 'group');
  if (!group) return null;
  const gx = group.position.x;
  const gy = group.position.y;
  const out: WorkshopFlowNode[] = [];
  for (const n of nodes) {
    if (n.id === groupId) continue;
    if (n.parentId === groupId) {
      const freed = { ...n, data: { ...n.data }, position: { x: n.position.x + gx, y: n.position.y + gy } } as WorkshopFlowNode;
      delete freed.parentId;
      delete freed.extent;
      out.push(freed);
    } else {
      out.push(n);
    }
  }
  return orderNodesParentFirst(out);
}

/** Ids of a group node and all its members (for delete-with-children). */
export function groupMemberIds(nodes: WorkshopFlowNode[], groupId: string): string[] {
  return [groupId, ...nodes.filter((n) => n.parentId === groupId).map((n) => n.id)];
}

/**
 * Clone a set of nodes (and the edges wholly between them) with fresh ids and a
 * pixel offset — used by copy/paste and duplicate. Group parenting is preserved
 * when both a group and its members are in the set (members keep their relative
 * position so they ride along with the offset group); a member whose parent is
 * NOT in the set is promoted to a free node so no dangling `parentId` survives.
 * A promoted member's parent-relative position is converted to absolute (via the
 * optional `allNodes` graph, which supplies the parent's position) before the
 * paste offset, so the clone lands beside the original instead of near the origin.
 * Returns the id remap so callers can select the clones.
 */
export function cloneNodesWithEdges(
  nodes: WorkshopFlowNode[],
  edges: WorkshopFlowEdge[],
  offset: XY,
  allNodes?: WorkshopFlowNode[]
): { nodes: WorkshopFlowNode[]; edges: WorkshopFlowEdge[]; idMap: Map<string, string> } {
  const idMap = new Map<string, string>();
  const inSet = new Set(nodes.map((n) => n.id));
  for (const n of nodes) idMap.set(n.id, newNodeId());

  // Absolute position of any node that could be an out-of-set parent (groups are
  // always free, so their `position` is already absolute).
  const posLookup = allNodes ? new Map(allNodes.map((n) => [n.id, n.position])) : null;

  const cloned = nodes.map((n) => {
    const parentKept = n.parentId && inSet.has(n.parentId);
    let position: XY;
    if (parentKept) {
      // Members with an in-set parent keep their (relative) position so they stay
      // put inside the offset group.
      position = { x: n.position.x, y: n.position.y };
    } else if (n.parentId) {
      // Orphan promotion: the node loses its group, so its relative position must
      // become absolute (parent origin + relative) before shifting by the offset.
      const parentPos = posLookup?.get(n.parentId);
      const baseX = parentPos ? parentPos.x + n.position.x : n.position.x;
      const baseY = parentPos ? parentPos.y + n.position.y : n.position.y;
      position = { x: baseX + offset.x, y: baseY + offset.y };
    } else {
      position = { x: n.position.x + offset.x, y: n.position.y + offset.y };
    }
    const next = {
      ...n,
      id: idMap.get(n.id) as string,
      selected: true,
      dragging: false,
      measured: undefined,
      position,
      data: { ...n.data },
    } as WorkshopFlowNode;
    if (parentKept) {
      next.parentId = idMap.get(n.parentId as string) as string;
      next.extent = 'parent';
    } else {
      delete next.parentId;
      delete next.extent;
    }
    return next;
  }) as WorkshopFlowNode[];

  const clonedEdges = edges
    .filter((e) => inSet.has(e.source) && inSet.has(e.target))
    .map((e) => ({
      id: newEdgeId(),
      source: idMap.get(e.source) as string,
      target: idMap.get(e.target) as string,
    }));
  return { nodes: cloned, edges: clonedEdges, idMap };
}
