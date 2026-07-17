/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Creative Workshop (创意工坊) shared types.
 *
 * Two layers, per the M0 contract (`docs/superpowers/specs/2026-07-05-creative-workshop-m0-contracts.md`):
 *
 * 1. **Wire types (§3)** — the REST payloads exchanged with `nomifun-workshop` /
 *    `nomifun-creation`. Field names are **snake_case**, mirroring the backend
 *    JSON exactly.
 * 2. **Canvas doc types (§4)** — the frontend-owned shape of `canvas.json`.
 *    The backend leaves node payload semantics to the frontend, while strictly
 *    validating the durable `wsn_`/`wse_` identity envelope and node
 *    references. Doc field names are **camelCase**, per the contract.
 *
 * Both layers are frozen at M0: downstream modules may **append** fields but must
 * not change existing semantics.
 */

import type {
  AssetId,
  CanvasId,
  CreationTaskId,
  ProviderId,
  WorkshopEdgeId,
  WorkshopNodeId,
} from '@/common/types/ids';

// ─────────────────────────────────────────────────────────────────────────────
// §3 Wire types — REST payloads (snake_case)
// ─────────────────────────────────────────────────────────────────────────────

/** Asset media kind. */
export type WorkshopAssetKind = 'image' | 'video' | 'text';

/**
 * Append-only (asset-library page): result-ordering token for
 * {@link ListAssetsQuery.sort}. Mirrors the backend `sort` param; an unknown /
 * absent value falls back to newest-created first server-side.
 */
export type AssetSortKey = 'created_desc' | 'created_asc' | 'updated_desc' | 'name_asc' | 'size_desc';

/** Media generation capability a task exercises. */
export type MediaCapability = 't2i' | 'i2i' | 'inpaint' | 't2v' | 'i2v' | 'v2v' | 'tts' | 'text';

/** Generation task lifecycle state. */
export type CreationTaskStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'canceled';

/** Role an input asset plays in a generation request. */
export type CreationInputRole = 'reference' | 'mask' | 'first_frame' | 'last_frame' | 'video' | 'audio';

/** Canvas gallery index row (§3.1 `WorkshopCanvasMeta`). */
export interface WorkshopCanvasMeta {
  id: CanvasId;
  title: string;
  /** Serve URL for the canvas thumbnail, or null when none has been rendered. */
  thumbnail_url: string | null;
  node_count: number;
  created_at: number;
  updated_at: number;
}

/** Provenance recorded on a generated asset (§2 `origin`). */
export interface WorkshopAssetOrigin {
  prompt?: string;
  model?: string;
  provider_id?: ProviderId;
  params?: Record<string, unknown>;
  canvas_id?: CanvasId;
  node_id?: WorkshopNodeId;
  task_id?: CreationTaskId;
}

/** Asset library / canvas-internal media record (§3.2 `WorkshopAsset`). */
export interface WorkshopAsset {
  id: AssetId;
  kind: WorkshopAssetKind;
  title: string;
  collection: string | null;
  tags: string[];
  mime: string | null;
  width: number | null;
  height: number | null;
  bytes: number | null;
  in_library: boolean;
  /** Text body for `kind = "text"` assets, else null. */
  text_content: string | null;
  origin: WorkshopAssetOrigin | null;
  /** Binary serve URL — `/api/workshop/files/{id}`. */
  url: string;
  /** Thumbnail serve URL, or null when none. */
  thumb_url: string | null;
  created_at: number;
  updated_at: number;
}

/** Structured generation error (§3.3 `error`). */
export interface CreationError {
  kind: string;
  message: string;
  http_status?: number;
}

/** Generation task record (§3.3 `CreationTask`). */
export interface CreationTask {
  id: CreationTaskId;
  canvas_id: CanvasId | null;
  node_id: WorkshopNodeId | null;
  provider_id: ProviderId;
  model: string;
  capability: MediaCapability;
  params: Record<string, unknown>;
  status: CreationTaskStatus;
  error: CreationError | null;
  result_asset_ids: AssetId[];
  attempt: number;
  submitted_at: number;
  started_at: number | null;
  finished_at: number | null;
}

/** One input asset attached to a generation request (§3.3 `inputs[]`). */
export interface CreationInput {
  asset_id: AssetId;
  role: CreationInputRole;
}

// ─── Request bodies / query params (snake_case) ──────────────────────────────

/** `POST /api/workshop/canvases` body. */
export interface CreateCanvasBody {
  title?: string;
}

/** `PATCH /api/workshop/canvases/{id}` body. */
export interface PatchCanvasBody {
  title: string;
}

/** `GET /api/workshop/assets` query params. */
export interface ListAssetsQuery {
  kind?: WorkshopAssetKind;
  collection?: string;
  q?: string;
  in_library?: boolean;
  /**
   * Append-only (M10a): `true` returns only assets with no collection
   * (server-side `collection IS NULL OR ''`). Mutually exclusive with
   * `collection` — the backend ignores `collection` when this is set.
   */
  ungrouped?: boolean;
  /** Append-only (asset-library page): exact-match filter on one tag. */
  tag?: string;
  /** Append-only (asset-library page): result ordering (default `created_desc`). */
  sort?: AssetSortKey;
  page?: number;
  page_size?: number;
}

/** `POST /api/workshop/assets` body — registers a text asset. */
export interface CreateTextAssetBody {
  kind: 'text';
  title: string;
  text_content: string;
  collection?: string;
  tags?: string[];
}

/** `PATCH /api/workshop/assets/{id}` body — partial edit. */
export interface PatchAssetBody {
  title?: string;
  collection?: string | null;
  tags?: string[];
  in_library?: boolean;
}

/** Optional metadata attached to a multipart asset upload. */
export interface UploadAssetOptions {
  title?: string;
  collection?: string;
  tags?: string[];
  in_library?: boolean;
}

/** `POST /api/creation/tasks` body. */
export interface CreateTaskBody {
  canvas_id?: CanvasId;
  node_id?: WorkshopNodeId;
  provider_id: ProviderId;
  /** Client-only platform discriminator; stripped before the HTTP request. */
  model: string;
  capability: MediaCapability;
  params: Record<string, unknown>;
  inputs: CreationInput[];
}

/** `GET /api/creation/tasks` query params. */
export interface ListTasksQuery {
  canvas_id?: CanvasId;
  status?: CreationTaskStatus;
  limit?: number;
}

// ─── Response envelopes ──────────────────────────────────────────────────────

/** `GET /api/workshop/canvases/{id}` response. */
export interface CanvasDetailResponse {
  meta: WorkshopCanvasMeta;
  doc: WorkshopCanvasDoc;
}

/** `PUT /api/workshop/canvases/{id}/doc` response. */
export interface PutDocResponse {
  updated_at: number;
}

/** `GET /api/workshop/assets` response. */
export interface ListAssetsResponse {
  items: WorkshopAsset[];
  total: number;
}

// ─────────────────────────────────────────────────────────────────────────────
// §4 Canvas doc types — frontend-owned, backend-opaque (camelCase)
// ─────────────────────────────────────────────────────────────────────────────

/** Current canvas-doc schema version. Bump when doc semantics change. */
export const WORKSHOP_DOC_SCHEMA = 1 as const;

/** Node kinds. `loop`/`compare`/`output`/`group` are placeholders (data shape lands in M8). */
export type WorkshopNodeKind = 'image' | 'text' | 'video' | 'generator' | 'loop' | 'compare' | 'output' | 'group';

/** Canvas background style. */
export type WorkshopCanvasBackground = 'dots' | 'lines' | 'blank';

/** Generator card mode. */
export type WorkshopGeneratorMode = 'image' | 'text' | 'video';

/** Generator card runtime status (independent of the backend task state). */
export type WorkshopGeneratorStatus = 'idle' | 'queued' | 'running' | 'success' | 'error';

/** Image node payload. */
export interface WorkshopImageNodeData {
  assetId: AssetId | null;
  naturalWidth?: number;
  naturalHeight?: number;
  caption?: string;
  /**
   * Append-only (M1): whether the resize handle keeps the image's aspect ratio.
   * Absent/undefined is treated as locked (default) by the canvas. Adding this
   * optional field does not change any existing semantics.
   */
  lockAspect?: boolean;
}

/** Text node payload. */
export interface WorkshopTextNodeData {
  content: string;
  fontSize?: number;
}

/** Video node payload. */
export interface WorkshopVideoNodeData {
  assetId: AssetId | null;
  durationMs?: number;
}

/** Batch-group state carried by a generator card that produced multiple images. */
export interface WorkshopGeneratorBatch {
  expanded: boolean;
  primary?: AssetId;
}

/** Generation card payload. */
export interface WorkshopGeneratorNodeData {
  mode: WorkshopGeneratorMode;
  providerId?: ProviderId;
  model?: string;
  prompt: string;
  params: Record<string, unknown>;
  mentions: string[];
  status: WorkshopGeneratorStatus;
  taskId?: CreationTaskId | null;
  resultAssetIds: AssetId[];
  batch?: WorkshopGeneratorBatch;
  errorMessage?: string;
  /**
   * Append-only (M7): mask asset (`wsa_…`) for local-repaint (inpaint) cards
   * spawned from the image editor's mask tool. Present ⇒ image-mode runs derive
   * the `inpaint` capability and attach the mask as a `role: 'mask'` input.
   */
  maskAssetId?: AssetId;
  /**
   * Append-only (M7): transient flag set when a card is spawned mid-chain
   * (mask repaint / continuous-edit) so it should run itself once on mount.
   * The card clears the flag before dispatching so it never re-fires.
   */
  autoRun?: boolean;
}

/**
 * Placeholder payload for node kinds whose data shape is defined later (M8):
 * `loop`, `compare`, `output`, `group`. Kept as an open record so M8 can refine
 * it without breaking M0-era docs.
 */
export type WorkshopPlaceholderNodeData = Record<string, unknown>;

// ── M8 flow-node payloads (loop / compare / output / group) ──────────────────
// Append-only refinements of {@link WorkshopPlaceholderNodeData}. Each is a plain
// serialisable record (no callbacks) so it round-trips through the canvas doc and
// history; components read a node's `data` via a cast to the matching shape. The
// doc-level union keeps using the open `WorkshopPlaceholderNodeData` type, so
// these additions never invalidate an M0-era doc.

/** How a loop node dispatches its rounds. */
export type WorkshopLoopMode = 'serial' | 'parallel';

/**
 * Loop node payload — a controller that drives repeated runs of a downstream
 * generator card. Only the configuration persists; live run progress is kept in
 * an in-memory registry so it survives node remounts without polluting history.
 */
export interface WorkshopLoopNodeData {
  /** Total rounds to run (1–50). */
  count: number;
  /** 1-based index of the first upstream image the window starts at. */
  start: number;
  /** How many upstream images each round consumes. */
  batch: number;
  /** Serial = one round after the previous settles; parallel = rolling window (≤3). */
  loopMode: WorkshopLoopMode;
  /** Count-injection template prepended to each round's prompt (`{i}` ⇒ round no.). */
  countTemplate: string;
}

/** Compare node payload — the A/B wipe divider position (0–1). Transient by design. */
export interface WorkshopCompareNodeData {
  split?: number;
}

/** Output node payload — an optional caption for the mid-chain inspector. */
export interface WorkshopOutputNodeData {
  label?: string;
}

/** Group node payload — the group's editable title. */
export interface WorkshopGroupNodeData {
  title: string;
}

/** Discriminated union over the payload for every node kind. */
export type WorkshopNodeData =
  | WorkshopImageNodeData
  | WorkshopTextNodeData
  | WorkshopVideoNodeData
  | WorkshopGeneratorNodeData
  | WorkshopPlaceholderNodeData;

/** Fields shared by every node. */
interface WorkshopNodeBase {
  id: WorkshopNodeId;
  x: number;
  y: number;
  w: number;
  h: number;
  /** Owning group node id, when the node belongs to a group (M8). */
  groupId?: WorkshopNodeId | null;
}

export interface WorkshopImageNode extends WorkshopNodeBase {
  kind: 'image';
  data: WorkshopImageNodeData;
}
export interface WorkshopTextNode extends WorkshopNodeBase {
  kind: 'text';
  data: WorkshopTextNodeData;
}
export interface WorkshopVideoNode extends WorkshopNodeBase {
  kind: 'video';
  data: WorkshopVideoNodeData;
}
export interface WorkshopGeneratorNode extends WorkshopNodeBase {
  kind: 'generator';
  data: WorkshopGeneratorNodeData;
}
export interface WorkshopPlaceholderNode extends WorkshopNodeBase {
  kind: 'loop' | 'compare' | 'output' | 'group';
  data: WorkshopPlaceholderNodeData;
}

/** A node on the canvas — discriminated by `kind`. */
export type WorkshopNode =
  | WorkshopImageNode
  | WorkshopTextNode
  | WorkshopVideoNode
  | WorkshopGeneratorNode
  | WorkshopPlaceholderNode;

/** A directed connection between two nodes. */
export interface WorkshopEdge {
  id: WorkshopEdgeId;
  from: WorkshopNodeId;
  to: WorkshopNodeId;
}

/** Canvas viewport (pan + zoom). */
export interface WorkshopViewport {
  x: number;
  y: number;
  zoom: number;
}

/** The full canvas document — payload semantics are owned by the frontend. */
export interface WorkshopCanvasDoc {
  schema: typeof WORKSHOP_DOC_SCHEMA;
  viewport: WorkshopViewport;
  background: WorkshopCanvasBackground;
  nodes: WorkshopNode[];
  edges: WorkshopEdge[];
}

/** Factory for a fresh, empty canvas document. */
export function createEmptyCanvasDoc(): WorkshopCanvasDoc {
  return {
    schema: WORKSHOP_DOC_SCHEMA,
    viewport: { x: 0, y: 0, zoom: 1 },
    background: 'dots',
    nodes: [],
    edges: [],
  };
}
