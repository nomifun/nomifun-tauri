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
  WorkshopGeneratorMode,
  WorkshopGeneratorNodeData,
  WorkshopImageNodeData,
  WorkshopNode,
  WorkshopNodeKind,
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
export type PlaceholderNodeData = Record<string, unknown>;

export type ImageFlowNode = Node<ImageNodeData, 'image'>;
export type TextFlowNode = Node<TextNodeData, 'text'>;
export type VideoFlowNode = Node<VideoNodeData, 'video'>;
export type GeneratorFlowNode = Node<GeneratorNodeData, 'generator'>;
export type PlaceholderFlowNode = Node<PlaceholderNodeData, 'loop' | 'compare' | 'output' | 'group'>;

/** Any node the workshop canvas can render. */
export type WorkshopFlowNode =
  | ImageFlowNode
  | TextFlowNode
  | VideoFlowNode
  | GeneratorFlowNode
  | PlaceholderFlowNode;

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

/** Convert a persisted doc into react-flow nodes + edges. */
export function docToFlow(doc: WorkshopCanvasDoc): { nodes: WorkshopFlowNode[]; edges: WorkshopFlowEdge[] } {
  const nodes: WorkshopFlowNode[] = doc.nodes.map((n) => {
    const { width, height } = sizeFor(n);
    return {
      id: n.id,
      type: n.kind,
      position: { x: n.x, y: n.y },
      width,
      height,
      data: { ...(n.data as Record<string, unknown>) },
    } as WorkshopFlowNode;
  });

  const nodeIds = new Set(nodes.map((n) => n.id));
  const edges: WorkshopFlowEdge[] = doc.edges
    .filter((e) => nodeIds.has(e.from) && nodeIds.has(e.to))
    .map((e) => ({ id: e.id, source: e.from, target: e.to }));

  return { nodes, edges };
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
  return {
    schema: WORKSHOP_DOC_SCHEMA,
    viewport,
    background,
    nodes: nodes.map((n) => {
      const { width, height } = nodeSize(n);
      return {
        id: n.id,
        kind: n.type as WorkshopNodeKind,
        x: Math.round(n.position.x),
        y: Math.round(n.position.y),
        w: width,
        h: height,
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
        data: { ...n.data },
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
      data: { ...n.data },
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

export function makePlaceholderNode(
  position: XY,
  kind: 'loop' | 'compare' | 'output' | 'group'
): PlaceholderFlowNode {
  const b = base(kind, position);
  return { id: b.id, type: kind, position: b.position, width: b.width, height: b.height, data: {} };
}

/**
 * Clone a set of nodes (and the edges wholly between them) with fresh ids and a
 * pixel offset — used by copy/paste and duplicate. Returns the id remap so
 * callers can select the clones.
 */
export function cloneNodesWithEdges(
  nodes: WorkshopFlowNode[],
  edges: WorkshopFlowEdge[],
  offset: XY
): { nodes: WorkshopFlowNode[]; edges: WorkshopFlowEdge[]; idMap: Map<string, string> } {
  const idMap = new Map<string, string>();
  const cloned = nodes.map((n) => {
    const id = newNodeId();
    idMap.set(n.id, id);
    return {
      ...n,
      id,
      selected: true,
      dragging: false,
      measured: undefined,
      position: { x: n.position.x + offset.x, y: n.position.y + offset.y },
      data: { ...n.data },
    };
  }) as WorkshopFlowNode[];
  const selected = new Set(nodes.map((n) => n.id));
  const clonedEdges = edges
    .filter((e) => selected.has(e.source) && selected.has(e.target))
    .map((e) => ({
      id: newEdgeId(),
      source: idMap.get(e.source) as string,
      target: idMap.get(e.target) as string,
    }));
  return { nodes: cloned, edges: clonedEdges, idMap };
}
