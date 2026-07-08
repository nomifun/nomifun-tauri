/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Node/edge spawning for generation results, driven imperatively through the
 * react-flow instance. `addNodes` / `addEdges` fire `onNodesChange` /
 * `onEdgesChange` in controlled mode (verified against @xyflow/react 12.x), so
 * new elements flow into the CanvasEditor's authoritative state — and therefore
 * into history + autosave — exactly like user edits.
 */

import type { ReactFlowInstance } from '@xyflow/react';
import {
  makeGeneratorNode,
  makeImageNode,
  makeTextNode,
  newEdgeId,
  type WorkshopFlowEdge,
  type WorkshopFlowNode,
} from '../canvas/model';
import { mentionRefForNode } from './pipeline';

type RF = ReactFlowInstance<WorkshopFlowNode, WorkshopFlowEdge>;

const GRID_CELL = 176;
const GRID_GAP = 22;
const RIGHT_GAP = 64;

/** Resolve a node's absolute canvas position (accounting for a group parent). */
function absolutePositionOf(rf: RF, node: WorkshopFlowNode): { x: number; y: number } {
  if (node.parentId) {
    const parent = rf.getNode(node.parentId);
    if (parent) return { x: parent.position.x + node.position.x, y: parent.position.y + node.position.y };
  }
  return { x: node.position.x, y: node.position.y };
}

/**
 * Origin (in absolute canvas coords) just right of the card. Spawned result /
 * continue / text nodes are added as free (parent-less) nodes, so the origin
 * must be absolute — a grouped card's own `position` is parent-relative.
 */
function rightOf(rf: RF, card: WorkshopFlowNode): { x: number; y: number; width: number } {
  const width = card.width ?? card.measured?.width ?? 344;
  const origin = absolutePositionOf(rf, card);
  return { x: origin.x + width + RIGHT_GAP, y: origin.y, width };
}

/**
 * Fan out a batch of result images into a grid of image nodes to the card's
 * right, each wired from the card. Used when a run yields more than one image.
 */
export function spawnResultNodes(rf: RF, card: WorkshopFlowNode, assetIds: string[]): void {
  if (assetIds.length === 0) return;
  const origin = rightOf(rf, card);
  const cols = Math.min(3, Math.ceil(Math.sqrt(assetIds.length)));
  const created: WorkshopFlowNode[] = [];
  const edges: WorkshopFlowEdge[] = [];
  assetIds.forEach((assetId, i) => {
    const col = i % cols;
    const row = Math.floor(i / cols);
    const pos = { x: origin.x + col * (GRID_CELL + GRID_GAP), y: origin.y + row * (GRID_CELL + GRID_GAP) };
    const node = makeImageNode(pos, { assetId });
    created.push(node);
    edges.push({ id: newEdgeId(), source: card.id, target: node.id });
  });
  rf.addNodes(created);
  rf.addEdges(edges);
}

/**
 * Spawn a downstream generation card seeded to continue-edit this card's result:
 * same model, the given instruction as its prompt, this card referenced via a
 * mention, and `autoRun` so it fires itself on mount. Returns the new node id.
 */
export function spawnContinueCard(
  rf: RF,
  card: WorkshopFlowNode,
  opts: { instruction: string; providerId?: string; model?: string; mode: 'image' | 'video' }
): void {
  const origin = rightOf(rf, card);
  const pos = { x: origin.x, y: origin.y };
  const node = makeGeneratorNode(pos, opts.mode, {
    prompt: opts.instruction,
    providerId: opts.providerId,
    model: opts.model,
    mentions: [mentionRefForNode(card.id)],
    autoRun: true,
  });
  rf.addNodes(node);
  rf.addEdges({ id: newEdgeId(), source: card.id, target: node.id });
}

/** Materialise a text result into a standalone text node wired from the card. */
export function spawnTextNode(rf: RF, card: WorkshopFlowNode, content: string): void {
  const origin = rightOf(rf, card);
  const node = makeTextNode({ x: origin.x, y: origin.y }, { content });
  rf.addNodes(node);
  rf.addEdges({ id: newEdgeId(), source: card.id, target: node.id });
}
