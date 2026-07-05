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
 * 2. **Canvas doc types (§4)** — the shape of the opaque `canvas.json` the
 *    backend only stores/serves. This is **frontend-owned**; the backend never
 *    parses it (it only measures `node_count` and enforces a size cap). Doc
 *    field names are **camelCase**, per the contract.
 *
 * Both layers are frozen at M0: downstream modules may **append** fields but must
 * not change existing semantics.
 */

// ─────────────────────────────────────────────────────────────────────────────
// §3 Wire types — REST payloads (snake_case)
// ─────────────────────────────────────────────────────────────────────────────

/** Asset media kind. */
export type WorkshopAssetKind = 'image' | 'video' | 'text';

/** Media generation capability a task exercises. */
export type MediaCapability = 't2i' | 'i2i' | 'inpaint' | 't2v' | 'i2v' | 'v2v' | 'tts' | 'text';

/** Generation task lifecycle state. */
export type CreationTaskStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'canceled';

/** Role an input asset plays in a generation request. */
export type CreationInputRole = 'reference' | 'mask' | 'first_frame' | 'last_frame' | 'video' | 'audio';

/** Canvas gallery index row (§3.1 `WorkshopCanvasMeta`). */
export interface WorkshopCanvasMeta {
  id: string;
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
  provider_id?: string;
  params?: Record<string, unknown>;
  canvas_id?: string;
  node_id?: string;
  task_id?: string;
}

/** Asset library / canvas-internal media record (§3.2 `WorkshopAsset`). */
export interface WorkshopAsset {
  id: string;
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
  id: string;
  canvas_id: string | null;
  node_id: string | null;
  provider_id: string;
  model: string;
  capability: MediaCapability;
  params: Record<string, unknown>;
  status: CreationTaskStatus;
  error: CreationError | null;
  result_asset_ids: string[];
  attempt: number;
  submitted_at: number;
  started_at: number | null;
  finished_at: number | null;
}

/** One input asset attached to a generation request (§3.3 `inputs[]`). */
export interface CreationInput {
  asset_id: string;
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
  canvas_id?: string;
  node_id?: string;
  provider_id: string;
  model: string;
  capability: MediaCapability;
  params: Record<string, unknown>;
  inputs: CreationInput[];
}

/** `GET /api/creation/tasks` query params. */
export interface ListTasksQuery {
  canvas_id?: string;
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
  assetId: string | null;
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
  assetId: string | null;
  durationMs?: number;
}

/** Batch-group state carried by a generator card that produced multiple images. */
export interface WorkshopGeneratorBatch {
  expanded: boolean;
  primary?: string;
}

/** Generation card payload. */
export interface WorkshopGeneratorNodeData {
  mode: WorkshopGeneratorMode;
  providerId?: string;
  model?: string;
  prompt: string;
  params: Record<string, unknown>;
  mentions: string[];
  status: WorkshopGeneratorStatus;
  taskId?: string | null;
  resultAssetIds: string[];
  batch?: WorkshopGeneratorBatch;
  errorMessage?: string;
}

/**
 * Placeholder payload for node kinds whose data shape is defined later (M8):
 * `loop`, `compare`, `output`, `group`. Kept as an open record so M8 can refine
 * it without breaking M0-era docs.
 */
export type WorkshopPlaceholderNodeData = Record<string, unknown>;

/** Discriminated union over the payload for every node kind. */
export type WorkshopNodeData =
  | WorkshopImageNodeData
  | WorkshopTextNodeData
  | WorkshopVideoNodeData
  | WorkshopGeneratorNodeData
  | WorkshopPlaceholderNodeData;

/** Fields shared by every node. */
interface WorkshopNodeBase {
  id: string;
  x: number;
  y: number;
  w: number;
  h: number;
  /** Owning group node id, when the node belongs to a group (M8). */
  groupId?: string | null;
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
  id: string;
  from: string;
  to: string;
}

/** Canvas viewport (pan + zoom). */
export interface WorkshopViewport {
  x: number;
  y: number;
  zoom: number;
}

/** The full canvas document — opaque to the backend, owned by the frontend. */
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
