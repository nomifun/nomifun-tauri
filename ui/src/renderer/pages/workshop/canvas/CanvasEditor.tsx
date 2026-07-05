/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * CanvasEditor — the `/workshop/:id` infinite-canvas editor body.
 *
 * Wraps `@xyflow/react` with the full P0 interaction set: pan / mouse-anchored
 * zoom / box + multi select / free drag / anchor-drag connect (with drop-in-
 * empty-space quick-create) / right-click menus / copy-paste (internal +
 * system clipboard) / delete / snapshot undo-redo / minimap / zoom bar /
 * background styles / drag-drop + asset-library insert / image preview / image
 * editor hand-off. State lives in react-flow's native shape and is converted to
 * / from the frozen canvas doc on load / debounced autosave.
 *
 * ── Slots for later modules ──────────────────────────────────────────────────
 *  - M4 asset library: `AssetsPanel` (mounted below) drives `handleInsertAsset`.
 *  - M5 image editor: `openImageEditor` result handling lives in `editImageNode`.
 *  - M7 generation: `GeneratorNode` is a shell; run/param wiring reads/writes its
 *    `data` via `updateNodeData` — no canvas changes needed.
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Background,
  BackgroundVariant,
  MiniMap,
  Panel,
  ReactFlow,
  ReactFlowProvider,
  addEdge,
  useEdgesState,
  useNodesState,
  useReactFlow,
  type Connection,
  type OnConnectEnd,
  type OnConnectStart,
  type Viewport,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';
import './canvas.css';
import { CopyOne, DeleteFour, MagicWand, Pic, Text, VideoTwo } from '@icon-park/react';
import { useTranslation } from 'react-i18next';
import { useArcoMessage } from '@renderer/utils/ui/useArcoMessage';
import AssetsPanel from '../assets/AssetsPanel';
import { openImageEditor, type ImageEditorMode } from '../editor';
import { patchAsset, uploadAsset } from '../api';
import { readAssetDrag, type WorkshopAssetDragPayload } from '../lib/dnd';
import { loadWorkshopMedia, revokeWorkshopMedia } from '../lib/media';
import type { WorkshopAsset, WorkshopCanvasBackground, WorkshopCanvasDoc } from '../types';
import { CanvasNodeContext, type CanvasNodeApi } from './CanvasNodeContext';
import { useCanvasHistory } from './history';
import { isImageFile, isVideoFile, pickFiles, readImageSize } from './media';
import { useDocPersistence, type SaveState } from './persistence';
import { useFlowColors, useThemeMode } from './theme';
import { minimapColorForKind } from './theme';
import {
  FIT_VIEW_OPTIONS,
  PASTE_OFFSET,
  ZOOM_MAX,
  ZOOM_MIN,
  buildSnapshot,
  cloneNodesWithEdges,
  docToFlow,
  flowToDoc,
  makeGeneratorNode,
  makeImageNode,
  makeTextNode,
  makeVideoNode,
  newEdgeId,
  snapshotToState,
  type CanvasSnapshot,
  type WorkshopFlowEdge,
  type WorkshopFlowNode,
  type XY,
} from './model';
import { WORKSHOP_NODE_TYPES } from './nodes';
import CanvasToolbar from './overlays/CanvasToolbar';
import FloatingMenu, { type MenuEntry } from './overlays/FloatingMenu';
import ImagePreview from './overlays/ImagePreview';
import ShortcutsHelp from './overlays/ShortcutsHelp';
import ZoomControls from './overlays/ZoomControls';

const BACKGROUND_CYCLE: WorkshopCanvasBackground[] = ['dots', 'lines', 'blank'];

const DEFAULT_EDGE_OPTIONS = { type: 'default' as const };

// react-flow modifier-key bindings (see the panning/selection notes in code).
const SELECTION_KEYS = ['Control', 'Meta'];
const MULTI_SELECT_KEYS = ['Shift', 'Control', 'Meta'];
const DELETE_KEYS = ['Delete', 'Backspace'];

interface MenuState {
  kind: 'pane' | 'node' | 'edge' | 'quick';
  x: number;
  y: number;
  flow?: XY;
  nodeId?: string;
  edgeId?: string;
  sourceId?: string;
}

function isEditableTarget(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el) return false;
  const tag = el.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el.isContentEditable;
}

export interface CanvasEditorProps {
  canvasId: string;
  initialDoc: WorkshopCanvasDoc;
  onSaveStateChange?: (state: SaveState) => void;
}

// ─────────────────────────────────────────────────────────────────────────────
// Inner canvas (inside ReactFlowProvider so it can use the flow hooks)
// ─────────────────────────────────────────────────────────────────────────────

const CanvasInner: React.FC<CanvasEditorProps> = ({ canvasId, initialDoc, onSaveStateChange }) => {
  const { t } = useTranslation();
  const [message, messageHolder] = useArcoMessage();
  const rf = useReactFlow<WorkshopFlowNode, WorkshopFlowEdge>();
  const theme = useThemeMode();
  const flowColors = useFlowColors(theme);
  const wrapperRef = useRef<HTMLDivElement | null>(null);

  const initial = useMemo(() => docToFlow(initialDoc), [initialDoc]);
  const [nodes, setNodes, onNodesChange] = useNodesState<WorkshopFlowNode>(initial.nodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState<WorkshopFlowEdge>(initial.edges);
  const [background, setBackground] = useState<WorkshopCanvasBackground>(initialDoc.background);

  const [menu, setMenu] = useState<MenuState | null>(null);
  const [helpOpen, setHelpOpen] = useState(false);
  const [assetsOpen, setAssetsOpen] = useState(false);
  const [preview, setPreview] = useState<{ assetIds: string[]; index: number } | null>(null);
  const [dropActive, setDropActive] = useState(false);

  // Live-state mirrors so the imperative history / save closures never go stale.
  const nodesRef = useRef(nodes);
  nodesRef.current = nodes;
  const edgesRef = useRef(edges);
  edgesRef.current = edges;
  const backgroundRef = useRef(background);
  backgroundRef.current = background;
  const viewportRef = useRef<Viewport>(initialDoc.viewport);

  const interactingRef = useRef(false);
  const applyingRef = useRef(false);
  const initializedRef = useRef(false);
  const connectSourceRef = useRef<string | null>(null);
  const clipboardRef = useRef<{ nodes: WorkshopFlowNode[]; edges: WorkshopFlowEdge[] } | null>(null);
  const pasteCountRef = useRef(0);

  const getSnapshot = useCallback(
    (): CanvasSnapshot => buildSnapshot(nodesRef.current, edgesRef.current, backgroundRef.current),
    []
  );
  const history = useCanvasHistory(getSnapshot);
  const historyRef = useRef(history);
  historyRef.current = history;

  const getDoc = useCallback(
    (): WorkshopCanvasDoc => flowToDoc(nodesRef.current, edgesRef.current, viewportRef.current, backgroundRef.current),
    []
  );
  const persistence = useDocPersistence(canvasId, getDoc, onSaveStateChange);
  const persistRef = useRef(persistence);
  persistRef.current = persistence;

  // Seed history baseline + last-saved signature once per canvas.
  useEffect(() => {
    historyRef.current.reset(buildSnapshot(initial.nodes, initial.edges, initialDoc.background));
    persistRef.current.markLoaded(initialDoc);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canvasId]);

  // Fit a freshly-opened, never-panned canvas that already has content.
  useEffect(() => {
    const vp = initialDoc.viewport;
    const pristine = vp.x === 0 && vp.y === 0 && vp.zoom === 1;
    if (pristine && initial.nodes.length > 0) {
      const raf = requestAnimationFrame(() => rf.fitView(FIT_VIEW_OPTIONS));
      // Capture the settled viewport once the fit animation finishes.
      const settle = window.setTimeout(() => {
        viewportRef.current = rf.getViewport();
      }, 320);
      return () => {
        cancelAnimationFrame(raf);
        window.clearTimeout(settle);
      };
    }
    return undefined;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canvasId]);

  // Record history + autosave whenever committed content changes.
  useEffect(() => {
    if (!initializedRef.current) {
      initializedRef.current = true;
      return;
    }
    if (applyingRef.current) {
      applyingRef.current = false;
      persistRef.current.schedule();
      return;
    }
    if (interactingRef.current) return; // handled on drag / resize end
    historyRef.current.record();
    persistRef.current.schedule();
  }, [nodes, edges, background]);

  // ── History application ─────────────────────────────────────────────────────

  const applySnapshot = useCallback(
    (snap: CanvasSnapshot | null) => {
      if (!snap) return;
      applyingRef.current = true;
      const next = snapshotToState(snap);
      setNodes(next.nodes);
      setEdges(next.edges);
      setBackground(next.background);
    },
    [setNodes, setEdges]
  );

  const undo = useCallback(() => applySnapshot(historyRef.current.undo()), [applySnapshot]);
  const redo = useCallback(() => applySnapshot(historyRef.current.redo()), [applySnapshot]);

  // ── Coordinate helpers ──────────────────────────────────────────────────────

  const wrapperXY = useCallback((clientX: number, clientY: number): XY => {
    const rect = wrapperRef.current?.getBoundingClientRect();
    return { x: clientX - (rect?.left ?? 0), y: clientY - (rect?.top ?? 0) };
  }, []);

  const viewportCenterFlow = useCallback((): XY => {
    const rect = wrapperRef.current?.getBoundingClientRect();
    if (!rect) return { x: 0, y: 0 };
    return rf.screenToFlowPosition({ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 });
  }, [rf]);

  // ── Node mutation primitives ────────────────────────────────────────────────

  const addNodes = useCallback(
    (created: WorkshopFlowNode[], newEdges: WorkshopFlowEdge[] = []) => {
      setNodes((ns) => [...ns.map((n) => (n.selected ? { ...n, selected: false } : n)), ...created]);
      if (newEdges.length) setEdges((es) => [...es, ...newEdges]);
    },
    [setNodes, setEdges]
  );

  const updateNodeData = useCallback(
    (nodeId: string, patch: Record<string, unknown>) => {
      setNodes((ns) =>
        ns.map((n) => (n.id === nodeId ? ({ ...n, data: { ...(n.data as Record<string, unknown>), ...patch } } as WorkshopFlowNode) : n))
      );
    },
    [setNodes]
  );

  const resizeNode = useCallback(
    (nodeId: string, size: { width: number; height: number }) => {
      setNodes((ns) => ns.map((n) => (n.id === nodeId ? ({ ...n, width: size.width, height: size.height } as WorkshopFlowNode) : n)));
    },
    [setNodes]
  );

  const removeNode = useCallback(
    (nodeId: string) => {
      setNodes((ns) => ns.filter((n) => n.id !== nodeId));
      setEdges((es) => es.filter((e) => e.source !== nodeId && e.target !== nodeId));
    },
    [setNodes, setEdges]
  );

  const duplicateNode = useCallback(
    (nodeId: string) => {
      const node = nodesRef.current.find((n) => n.id === nodeId);
      if (!node) return;
      const { nodes: cloned } = cloneNodesWithEdges([node], [], { x: PASTE_OFFSET, y: PASTE_OFFSET });
      addNodes(cloned);
    },
    [addNodes]
  );

  // ── Media / asset actions ───────────────────────────────────────────────────

  const uploadFile = useCallback(
    async (file: File): Promise<WorkshopAsset | null> => {
      try {
        return await uploadAsset(file, { in_library: false });
      } catch (e) {
        message.error(
          `${t('workshopCanvas.toast.uploadFailed', { defaultValue: '上传失败' })}: ${e instanceof Error ? e.message : String(e)}`
        );
        return null;
      }
    },
    [message, t]
  );

  const fillNodeFromFile = useCallback(
    async (nodeId: string, file: File): Promise<void> => {
      const node = nodesRef.current.find((n) => n.id === nodeId);
      if (!node) return;
      const asset = await uploadFile(file);
      if (!asset) return;
      if (node.type === 'image') {
        const size = isImageFile(file) ? await readImageSize(file) : null;
        updateNodeData(nodeId, {
          assetId: asset.id,
          naturalWidth: asset.width ?? size?.width,
          naturalHeight: asset.height ?? size?.height,
        });
      } else {
        updateNodeData(nodeId, { assetId: asset.id });
      }
    },
    [uploadFile, updateNodeData]
  );

  const canvasImageAssetIds = useCallback(
    (): string[] =>
      nodesRef.current
        .filter((n) => n.type === 'image' && typeof (n.data as { assetId?: unknown }).assetId === 'string')
        .map((n) => (n.data as { assetId: string }).assetId),
    []
  );

  const previewImageNode = useCallback(
    (nodeId: string) => {
      const ids = canvasImageAssetIds();
      const node = nodesRef.current.find((n) => n.id === nodeId);
      const assetId = node && (node.data as { assetId?: string }).assetId;
      const index = assetId ? Math.max(0, ids.indexOf(assetId)) : 0;
      if (ids.length) setPreview({ assetIds: ids, index });
    },
    [canvasImageAssetIds]
  );

  const saveAssetToLibrary = useCallback(
    async (assetId: string) => {
      try {
        await patchAsset(assetId, { in_library: true });
        message.success(t('workshopCanvas.toast.savedToLibrary', { defaultValue: '已存入资产库' }));
      } catch (e) {
        message.error(
          `${t('workshopCanvas.toast.saveToLibraryFailed', { defaultValue: '存入资产库失败' })}: ${e instanceof Error ? e.message : String(e)}`
        );
      }
    },
    [message, t]
  );

  const downloadAsset = useCallback(
    async (assetId: string, filename?: string) => {
      try {
        const url = await loadWorkshopMedia(assetId);
        const a = document.createElement('a');
        a.href = url;
        a.download = filename ?? assetId;
        document.body.appendChild(a);
        a.click();
        a.remove();
      } catch (e) {
        message.error(
          `${t('workshopCanvas.toast.downloadFailed', { defaultValue: '下载失败' })}: ${e instanceof Error ? e.message : String(e)}`
        );
      }
    },
    [message, t]
  );

  // ── Image editor hand-off (M5 provides the real modal) ──────────────────────

  const editImageNode = useCallback(
    async (nodeId: string, mode: ImageEditorMode) => {
      const node = nodesRef.current.find((n) => n.id === nodeId);
      if (!node || node.type !== 'image') return;
      const data = node.data as { assetId?: string; naturalWidth?: number; naturalHeight?: number };
      if (!data.assetId) return;
      if (mode === 'mask') {
        message.info(t('workshopCanvas.toast.maskDeferred', { defaultValue: '局部重绘将在生成卡片接通后可用' }));
        return;
      }
      let src: string;
      try {
        src = await loadWorkshopMedia(data.assetId);
      } catch {
        return;
      }
      const result = await openImageEditor({ mode, src, naturalWidth: data.naturalWidth, naturalHeight: data.naturalHeight });
      if (!result) return;

      if (result.type === 'crop' || result.type === 'upscale') {
        const file = new File([result.blob], `${result.type}.png`, { type: result.blob.type || 'image/png' });
        const asset = await uploadFile(file);
        if (!asset) return;
        revokeWorkshopMedia(data.assetId);
        updateNodeData(nodeId, { assetId: asset.id, naturalWidth: asset.width, naturalHeight: asset.height });
      } else if (result.type === 'split') {
        const cols = Math.max(1, ...result.pieces.map((p) => p.col + 1));
        const originX = node.position.x + (node.width ?? 240) + 60;
        const originY = node.position.y;
        const cell = 180;
        const created: WorkshopFlowNode[] = [];
        const newEdges: WorkshopFlowEdge[] = [];
        for (const piece of result.pieces) {
          const file = new File([piece.blob], `piece-${piece.row}-${piece.col}.png`, { type: piece.blob.type || 'image/png' });
          const asset = await uploadFile(file);
          if (!asset) continue;
          const pos = { x: originX + piece.col * (cell + 24), y: originY + piece.row * (cell + 24) };
          const imgNode = makeImageNode(pos, {
            assetId: asset.id,
            naturalWidth: asset.width ?? undefined,
            naturalHeight: asset.height ?? undefined,
          });
          created.push(imgNode);
          newEdges.push({ id: newEdgeId(), source: node.id, target: imgNode.id });
        }
        void cols;
        if (created.length) addNodes(created, newEdges);
      }
    },
    [message, t, uploadFile, updateNodeData, addNodes]
  );

  // ── Interaction gates (drag / resize) ───────────────────────────────────────

  const beginInteraction = useCallback(() => {
    interactingRef.current = true;
    historyRef.current.beginInteraction();
  }, []);
  const commitInteraction = useCallback(() => {
    interactingRef.current = false;
    historyRef.current.commitNow();
    persistRef.current.schedule();
  }, []);

  // ── Connect handlers ────────────────────────────────────────────────────────

  const onConnectStart: OnConnectStart = useCallback((_, params) => {
    connectSourceRef.current = params.nodeId ?? null;
  }, []);

  const onConnect = useCallback(
    (conn: Connection) => {
      setEdges((es) => addEdge({ ...conn, id: newEdgeId() }, es));
    },
    [setEdges]
  );

  const onConnectEnd: OnConnectEnd = useCallback(
    (event, connectionState) => {
      const source = connectionState.fromNode?.id ?? connectSourceRef.current;
      connectSourceRef.current = null;
      if (connectionState.toNode || !source) return; // landed on a node → onConnect handled it
      const point = 'changedTouches' in event ? event.changedTouches[0] : (event as MouseEvent);
      if (!point) return;
      const local = wrapperXY(point.clientX, point.clientY);
      const flow = rf.screenToFlowPosition({ x: point.clientX, y: point.clientY });
      setMenu({ kind: 'quick', x: local.x, y: local.y, flow, sourceId: source });
    },
    [rf, wrapperXY]
  );

  // ── Context menus ───────────────────────────────────────────────────────────

  const onPaneContextMenu = useCallback(
    (event: React.MouseEvent | MouseEvent) => {
      event.preventDefault();
      const local = wrapperXY(event.clientX, event.clientY);
      const flow = rf.screenToFlowPosition({ x: event.clientX, y: event.clientY });
      setMenu({ kind: 'pane', x: local.x, y: local.y, flow });
    },
    [rf, wrapperXY]
  );

  const onNodeContextMenu = useCallback(
    (event: React.MouseEvent, node: WorkshopFlowNode) => {
      event.preventDefault();
      const local = wrapperXY(event.clientX, event.clientY);
      setMenu({ kind: 'node', x: local.x, y: local.y, nodeId: node.id });
    },
    [wrapperXY]
  );

  const onEdgeContextMenu = useCallback(
    (event: React.MouseEvent, edge: WorkshopFlowEdge) => {
      event.preventDefault();
      const local = wrapperXY(event.clientX, event.clientY);
      setMenu({ kind: 'edge', x: local.x, y: local.y, edgeId: edge.id });
    },
    [wrapperXY]
  );

  // ── Create helpers used by menus / drops ────────────────────────────────────

  const createNodeFromAsset = useCallback(
    (asset: Pick<WorkshopAsset, 'id' | 'kind' | 'title' | 'width' | 'height'> | WorkshopAssetDragPayload, pos: XY) => {
      const assetId = 'asset_id' in asset ? asset.asset_id : asset.id;
      const kind = asset.kind;
      if (kind === 'image') {
        addNodes([makeImageNode(pos, { assetId, naturalWidth: asset.width ?? undefined, naturalHeight: asset.height ?? undefined })]);
      } else if (kind === 'video') {
        addNodes([makeVideoNode(pos, { assetId })]);
      } else {
        addNodes([makeTextNode(pos, { content: asset.title ?? '' })]);
      }
    },
    [addNodes]
  );

  const addImageViaUpload = useCallback(
    async (pos: XY) => {
      const files = await pickFiles('image/*', false);
      const file = files.find(isImageFile);
      if (!file) return;
      const asset = await uploadFile(file);
      if (!asset) return;
      const size = await readImageSize(file);
      addNodes([
        makeImageNode(pos, {
          assetId: asset.id,
          naturalWidth: asset.width ?? size?.width,
          naturalHeight: asset.height ?? size?.height,
        }),
      ]);
    },
    [uploadFile, addNodes]
  );

  const addVideoViaUpload = useCallback(
    async (pos: XY) => {
      const files = await pickFiles('video/*', false);
      const file = files.find(isVideoFile);
      if (!file) return;
      const asset = await uploadFile(file);
      if (!asset) return;
      addNodes([makeVideoNode(pos, { assetId: asset.id })]);
    },
    [uploadFile, addNodes]
  );

  const createAndConnect = useCallback(
    (factory: (pos: XY) => WorkshopFlowNode, pos: XY, sourceId: string | undefined) => {
      const node = factory(pos);
      const newEdges = sourceId ? [{ id: newEdgeId(), source: sourceId, target: node.id }] : [];
      addNodes([node], newEdges);
    },
    [addNodes]
  );

  // ── Selection / clipboard ───────────────────────────────────────────────────

  const selectAll = useCallback(() => {
    setNodes((ns) => ns.map((n) => (n.selected ? n : { ...n, selected: true })));
    setEdges((es) => es.map((e) => (e.selected ? e : { ...e, selected: true })));
  }, [setNodes, setEdges]);

  const clearSelection = useCallback(() => {
    setNodes((ns) => ns.map((n) => (n.selected ? { ...n, selected: false } : n)));
    setEdges((es) => es.map((e) => (e.selected ? { ...e, selected: false } : e)));
  }, [setNodes, setEdges]);

  const copySelection = useCallback((): boolean => {
    const sel = nodesRef.current.filter((n) => n.selected);
    if (!sel.length) return false;
    const ids = new Set(sel.map((n) => n.id));
    const between = edgesRef.current.filter((e) => ids.has(e.source) && ids.has(e.target));
    clipboardRef.current = {
      nodes: sel.map((n) => ({ ...n, selected: false, data: { ...(n.data as Record<string, unknown>) } }) as WorkshopFlowNode),
      edges: between.map((e) => ({ id: e.id, source: e.source, target: e.target })),
    };
    pasteCountRef.current = 0;
    return true;
  }, []);

  const pasteInternal = useCallback((): boolean => {
    const clip = clipboardRef.current;
    if (!clip || !clip.nodes.length) return false;
    pasteCountRef.current += 1;
    const off = PASTE_OFFSET * pasteCountRef.current;
    const { nodes: cn, edges: ce } = cloneNodesWithEdges(clip.nodes, clip.edges, { x: off, y: off });
    addNodes(cn, ce);
    return true;
  }, [addNodes]);

  const pasteFromSystem = useCallback(async () => {
    const center = viewportCenterFlow();
    try {
      if (navigator.clipboard && 'read' in navigator.clipboard) {
        const items = await navigator.clipboard.read();
        for (const item of items) {
          const imgType = item.types.find((ty) => ty.startsWith('image/'));
          if (imgType) {
            const blob = await item.getType(imgType);
            const file = new File([blob], 'pasted-image.png', { type: imgType });
            const asset = await uploadFile(file);
            if (asset) {
              const size = await readImageSize(blob);
              addNodes([
                makeImageNode(center, {
                  assetId: asset.id,
                  naturalWidth: asset.width ?? size?.width,
                  naturalHeight: asset.height ?? size?.height,
                }),
              ]);
            }
            return;
          }
        }
      }
      const text = await navigator.clipboard?.readText?.();
      if (text && text.trim()) addNodes([makeTextNode(center, { content: text })]);
    } catch {
      // Clipboard permission denied / unavailable — silently ignore.
    }
  }, [viewportCenterFlow, uploadFile, addNodes]);

  const handleInsertAsset = useCallback(
    (asset: WorkshopAsset) => {
      createNodeFromAsset(asset, viewportCenterFlow());
      setAssetsOpen(false);
    },
    [createNodeFromAsset, viewportCenterFlow]
  );

  // ── Drag & drop (files + library assets) ────────────────────────────────────

  const onDragOver = useCallback((e: React.DragEvent) => {
    const dt = e.dataTransfer;
    const hasPayload =
      Array.from(dt.types).includes('Files') || Array.from(dt.types).includes('application/x-nomifun-workshop-asset');
    if (!hasPayload) return;
    e.preventDefault();
    dt.dropEffect = 'copy';
    setDropActive(true);
  }, []);

  const onDragLeave = useCallback((e: React.DragEvent) => {
    if (!wrapperRef.current?.contains(e.relatedTarget as Node)) setDropActive(false);
  }, []);

  const onDrop = useCallback(
    async (e: React.DragEvent) => {
      e.preventDefault();
      setDropActive(false);
      const flow = rf.screenToFlowPosition({ x: e.clientX, y: e.clientY });

      const assetDrag = readAssetDrag(e.dataTransfer);
      if (assetDrag) {
        createNodeFromAsset(assetDrag, flow);
        return;
      }

      const files = Array.from(e.dataTransfer.files);
      let offset = 0;
      for (const file of files) {
        const pos = { x: flow.x + offset, y: flow.y + offset };
        if (isImageFile(file)) {
          const asset = await uploadFile(file);
          if (asset) {
            const size = await readImageSize(file);
            addNodes([
              makeImageNode(pos, {
                assetId: asset.id,
                naturalWidth: asset.width ?? size?.width,
                naturalHeight: asset.height ?? size?.height,
              }),
            ]);
          }
        } else if (isVideoFile(file)) {
          const asset = await uploadFile(file);
          if (asset) addNodes([makeVideoNode(pos, { assetId: asset.id })]);
        }
        offset += 28;
      }
    },
    [rf, createNodeFromAsset, uploadFile, addNodes]
  );

  // ── Keyboard shortcuts ──────────────────────────────────────────────────────

  const keyHandlerRef = useRef<(e: KeyboardEvent) => void>(() => {});
  keyHandlerRef.current = (e: KeyboardEvent) => {
    if (isEditableTarget(e.target)) return;
    const mod = e.ctrlKey || e.metaKey;

    if (mod && e.key.toLowerCase() === 'z') {
      e.preventDefault();
      if (e.shiftKey) redo();
      else undo();
      return;
    }
    if (mod && e.key.toLowerCase() === 'y') {
      e.preventDefault();
      redo();
      return;
    }
    if (mod && e.key.toLowerCase() === 'a') {
      e.preventDefault();
      selectAll();
      return;
    }
    if (mod && e.key.toLowerCase() === 'c') {
      if (copySelection()) e.preventDefault();
      return;
    }
    if (mod && e.key.toLowerCase() === 'v') {
      e.preventDefault();
      if (!pasteInternal()) void pasteFromSystem();
      return;
    }
    if (mod && e.key.toLowerCase() === 'd') {
      const sel = nodesRef.current.filter((n) => n.selected);
      if (sel.length) {
        e.preventDefault();
        const { nodes: cn, edges: ce } = cloneNodesWithEdges(
          sel,
          edgesRef.current.filter((edge) => sel.some((n) => n.id === edge.source) && sel.some((n) => n.id === edge.target)),
          { x: PASTE_OFFSET, y: PASTE_OFFSET }
        );
        addNodes(cn, ce);
      }
      return;
    }
    if (e.key === 'Escape') {
      setMenu(null);
      setHelpOpen(false);
      if (preview) setPreview(null);
      clearSelection();
      return;
    }
    if (!mod && (e.key === '?' || (e.key === '/' && e.shiftKey))) {
      e.preventDefault();
      setHelpOpen((v) => !v);
      return;
    }
    if (!mod && e.key.toLowerCase() === 'a') {
      e.preventDefault();
      setAssetsOpen((v) => !v);
    }
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent): void => keyHandlerRef.current(e);
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, []);

  // ── Viewport tracking ───────────────────────────────────────────────────────

  const onMove = useCallback((_: MouseEvent | TouchEvent | null, vp: Viewport) => {
    viewportRef.current = vp;
  }, []);
  const onMoveEnd = useCallback((_: MouseEvent | TouchEvent | null, vp: Viewport) => {
    viewportRef.current = vp;
    persistRef.current.schedule();
  }, []);

  // ── Node API context (stable across renders) ────────────────────────────────

  const nodeApi = useMemo<CanvasNodeApi>(
    () => ({
      theme,
      interactive: true,
      updateNodeData,
      resizeNode,
      removeNode,
      duplicateNode,
      fillNodeFromFile: (id, file) => void fillNodeFromFile(id, file),
      previewImageNode,
      saveAssetToLibrary: (id) => void saveAssetToLibrary(id),
      downloadAsset: (id, filename) => void downloadAsset(id, filename),
      editImageNode: (id, mode) => void editImageNode(id, mode),
      commitInteraction,
      beginInteraction,
    }),
    [
      theme,
      updateNodeData,
      resizeNode,
      removeNode,
      duplicateNode,
      fillNodeFromFile,
      previewImageNode,
      saveAssetToLibrary,
      downloadAsset,
      editImageNode,
      commitInteraction,
      beginInteraction,
    ]
  );

  // ── Context-menu entries ────────────────────────────────────────────────────

  const menuEntries = useMemo<MenuEntry[]>(() => {
    if (!menu) return [];
    if (menu.kind === 'node') {
      return [
        {
          type: 'item',
          key: 'duplicate',
          label: t('workshopCanvas.menu.duplicate', { defaultValue: '复制副本' }),
          icon: <CopyOne theme='outline' size={14} strokeWidth={3} />,
          onClick: () => menu.nodeId && duplicateNode(menu.nodeId),
        },
        {
          type: 'item',
          key: 'delete',
          label: t('workshopCanvas.menu.delete', { defaultValue: '删除' }),
          icon: <DeleteFour theme='outline' size={14} strokeWidth={3} />,
          danger: true,
          onClick: () => menu.nodeId && removeNode(menu.nodeId),
        },
      ];
    }
    if (menu.kind === 'edge') {
      return [
        {
          type: 'item',
          key: 'delete-edge',
          label: t('workshopCanvas.menu.deleteEdge', { defaultValue: '删除连线' }),
          icon: <DeleteFour theme='outline' size={14} strokeWidth={3} />,
          danger: true,
          onClick: () => menu.edgeId && setEdges((es) => es.filter((e) => e.id !== menu.edgeId)),
        },
      ];
    }
    // pane + quick both create nodes at menu.flow (quick also connects to source).
    const pos = menu.flow ?? { x: 0, y: 0 };
    const source = menu.kind === 'quick' ? menu.sourceId : undefined;
    const entries: MenuEntry[] = [
      { type: 'header', key: 'h', label: t('workshopCanvas.menu.newNode', { defaultValue: '在此新建节点' }) },
      {
        type: 'item',
        key: 'image',
        label:
          menu.kind === 'quick'
            ? t('workshopCanvas.menu.image', { defaultValue: '图片' })
            : t('workshopCanvas.menu.imageUpload', { defaultValue: '上传图片' }),
        icon: <Pic theme='outline' size={14} strokeWidth={3} />,
        onClick: () => {
          if (menu.kind === 'quick') createAndConnect((p) => makeImageNode(p), pos, source);
          else void addImageViaUpload(pos);
        },
      },
      {
        type: 'item',
        key: 'text',
        label: t('workshopCanvas.menu.text', { defaultValue: '文本' }),
        icon: <Text theme='outline' size={14} strokeWidth={3} />,
        onClick: () => createAndConnect((p) => makeTextNode(p), pos, source),
      },
      {
        type: 'item',
        key: 'video',
        label:
          menu.kind === 'quick'
            ? t('workshopCanvas.menu.video', { defaultValue: '视频' })
            : t('workshopCanvas.menu.videoUpload', { defaultValue: '上传视频' }),
        icon: <VideoTwo theme='outline' size={14} strokeWidth={3} />,
        onClick: () => {
          if (menu.kind === 'quick') createAndConnect((p) => makeVideoNode(p), pos, source);
          else void addVideoViaUpload(pos);
        },
      },
      {
        type: 'item',
        key: 'generator',
        label: t('workshopCanvas.menu.generator', { defaultValue: '生成卡片' }),
        icon: <MagicWand theme='outline' size={14} strokeWidth={3} />,
        onClick: () => createAndConnect((p) => makeGeneratorNode(p), pos, source),
      },
    ];
    if (menu.kind === 'pane') {
      entries.push({ type: 'divider', key: 'div' });
      entries.push({
        type: 'item',
        key: 'paste',
        label: t('workshopCanvas.menu.paste', { defaultValue: '粘贴' }),
        icon: <CopyOne theme='outline' size={14} strokeWidth={3} />,
        disabled: !clipboardRef.current?.nodes.length,
        onClick: () => {
          if (!pasteInternal()) void pasteFromSystem();
        },
      });
    }
    return entries;
  }, [
    menu,
    t,
    duplicateNode,
    removeNode,
    setEdges,
    createAndConnect,
    addImageViaUpload,
    addVideoViaUpload,
    pasteInternal,
    pasteFromSystem,
  ]);

  // ── Render ──────────────────────────────────────────────────────────────────

  return (
    <div
      ref={wrapperRef}
      className={['relative size-full min-h-0 overflow-hidden', dropActive ? 'nomi-ws-dropzone-active' : ''].join(' ')}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={(e) => void onDrop(e)}
    >
      {messageHolder}
      <CanvasNodeContext.Provider value={nodeApi}>
        <ReactFlow<WorkshopFlowNode, WorkshopFlowEdge>
          className='nomi-ws-flow'
          nodes={nodes}
          edges={edges}
          nodeTypes={WORKSHOP_NODE_TYPES}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          onConnectStart={onConnectStart}
          onConnectEnd={onConnectEnd}
          onNodeDragStart={beginInteraction}
          onNodeDragStop={commitInteraction}
          onSelectionDragStart={beginInteraction}
          onSelectionDragStop={commitInteraction}
          onNodeContextMenu={onNodeContextMenu}
          onEdgeContextMenu={onEdgeContextMenu}
          onPaneContextMenu={onPaneContextMenu}
          onPaneClick={() => setMenu(null)}
          onMove={onMove}
          onMoveEnd={onMoveEnd}
          defaultViewport={initialDoc.viewport}
          defaultEdgeOptions={DEFAULT_EDGE_OPTIONS}
          colorMode={theme}
          minZoom={ZOOM_MIN}
          maxZoom={ZOOM_MAX}
          proOptions={{ hideAttribution: true }}
          nodesConnectable
          nodesDraggable
          elementsSelectable
          zoomOnScroll
          zoomOnDoubleClick={false}
          panOnDrag={[0, 1]}
          selectionKeyCode={SELECTION_KEYS}
          multiSelectionKeyCode={MULTI_SELECT_KEYS}
          deleteKeyCode={DELETE_KEYS}
          connectionRadius={34}
          onlyRenderVisibleElements
          connectionLineStyle={{ stroke: 'rgb(var(--primary-6))', strokeWidth: 2.5 }}
          fitViewOptions={FIT_VIEW_OPTIONS}
        >
          <Background
            variant={
              background === 'lines'
                ? BackgroundVariant.Lines
                : background === 'blank'
                  ? BackgroundVariant.Dots
                  : BackgroundVariant.Dots
            }
            gap={background === 'lines' ? 28 : 22}
            size={background === 'blank' ? 0 : 1.4}
            color={background === 'lines' ? flowColors.lines : flowColors.dots}
          />
          <Panel position='top-right'>
            <CanvasToolbar
              canUndo={history.canUndo}
              canRedo={history.canRedo}
              onUndo={undo}
              onRedo={redo}
              background={background}
              onCycleBackground={() =>
                setBackground((b) => BACKGROUND_CYCLE[(BACKGROUND_CYCLE.indexOf(b) + 1) % BACKGROUND_CYCLE.length])
              }
              assetsOpen={assetsOpen}
              onToggleAssets={() => setAssetsOpen((v) => !v)}
              onAddNode={() => {
                const rect = wrapperRef.current?.getBoundingClientRect();
                const local = rect ? { x: rect.width / 2, y: rect.height / 2 } : { x: 200, y: 200 };
                setMenu({ kind: 'pane', x: local.x, y: local.y, flow: viewportCenterFlow() });
              }}
              onOpenHelp={() => setHelpOpen(true)}
            />
          </Panel>
          <Panel position='bottom-center'>
            <ZoomControls />
          </Panel>
          <MiniMap
            pannable
            zoomable
            position='bottom-right'
            maskColor={flowColors.minimapMask}
            style={{ background: flowColors.minimapBg, border: `1px solid ${flowColors.minimapStroke}` }}
            nodeColor={(n) => minimapColorForKind(String(n.type ?? ''), theme)}
            nodeStrokeWidth={2}
          />
        </ReactFlow>
      </CanvasNodeContext.Provider>

      {menu && <FloatingMenu x={menu.x} y={menu.y} entries={menuEntries} onClose={() => setMenu(null)} />}
      {helpOpen && <ShortcutsHelp onClose={() => setHelpOpen(false)} />}
      {preview && (
        <ImagePreview
          assetIds={preview.assetIds}
          startIndex={preview.index}
          onClose={() => setPreview(null)}
          onDownload={(id) => void downloadAsset(id)}
        />
      )}
      <AssetsPanel canvasId={canvasId} open={assetsOpen} onClose={() => setAssetsOpen(false)} onInsertAsset={handleInsertAsset} />
    </div>
  );
};

// ─────────────────────────────────────────────────────────────────────────────

const CanvasEditor: React.FC<CanvasEditorProps> = (props) => (
  <ReactFlowProvider>
    <CanvasInner {...props} />
  </ReactFlowProvider>
);

export default CanvasEditor;
