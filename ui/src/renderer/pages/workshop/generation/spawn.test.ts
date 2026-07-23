import { describe, expect, test } from 'bun:test';
import type { ReactFlowInstance } from '@xyflow/react';
import { parseAssetId, parseWorkshopNodeId, type AssetId } from '@/common/types/ids';
import type { WorkshopFlowEdge, WorkshopFlowNode } from '../canvas/model';
import { orderResultAssetIds } from './ResultView';
import { spawnResultNodes } from './spawn';

const asset = (label: string): AssetId => {
  const suffix = Array.from(label)
    .map((char) => char.charCodeAt(0).toString(16).padStart(2, '0'))
    .join('')
    .slice(0, 12)
    .padEnd(12, '0');
  return parseAssetId(`019b0000-0000-7000-8000-${suffix}`);
};

function harness(): {
  rf: ReactFlowInstance<WorkshopFlowNode, WorkshopFlowEdge>;
  card: WorkshopFlowNode;
  nodes: WorkshopFlowNode[];
  edges: WorkshopFlowEdge[];
} {
  const nodes: WorkshopFlowNode[] = [];
  const edges: WorkshopFlowEdge[] = [];
  const card = {
    id: parseWorkshopNodeId('019b0000-0000-7000-8000-000000000001'),
    type: 'generator',
    position: { x: 40, y: 60 },
    width: 344,
    data: { mode: 'image', prompt: '', params: {}, mentions: [], status: 'success', resultAssetIds: [] },
  } as WorkshopFlowNode;
  const rf = {
    getNode: () => undefined,
    addNodes: (input: WorkshopFlowNode | WorkshopFlowNode[]) => {
      nodes.push(...(Array.isArray(input) ? input : [input]));
    },
    addEdges: (input: WorkshopFlowEdge | WorkshopFlowEdge[]) => {
      edges.push(...(Array.isArray(input) ? input : [input]));
    },
  } as unknown as ReactFlowInstance<WorkshopFlowNode, WorkshopFlowEdge>;
  return { rf, card, nodes, edges };
}

describe('spawnResultNodes multi-artifact routing', () => {
  test('creates image nodes for every extra image artifact', async () => {
    const h = harness();
    const ids = [asset('image_a'), asset('image_b')];

    await spawnResultNodes(h.rf, h.card, 'image', ids);

    expect(h.nodes.map((node) => node.type)).toEqual(['image', 'image']);
    expect(h.nodes.map((node) => node.data.assetId)).toEqual(ids);
    expect(h.edges).toHaveLength(2);
  });

  test('creates video nodes instead of mislabelling video artifacts as images', async () => {
    const h = harness();
    const ids = [asset('video_a'), asset('video_b')];

    await spawnResultNodes(h.rf, h.card, 'video', ids);

    expect(h.nodes.map((node) => node.type)).toEqual(['video', 'video']);
    expect(h.nodes.map((node) => node.data.assetId)).toEqual(ids);
    expect(h.edges).toHaveLength(2);
  });

  test('loads text artifacts and materialises real text nodes', async () => {
    const h = harness();
    const ids = [asset('text_a'), asset('text_b')];

    await spawnResultNodes(h.rf, h.card, 'text', ids, {
      loadText: async (id) => `body:${id}`,
    });

    expect(h.nodes.map((node) => node.type)).toEqual(['text', 'text']);
    expect(h.nodes.map((node) => node.data.content)).toEqual(ids.map((id) => `body:${id}`));
    expect(h.nodes.map((node) => node.data.sourceAssetId)).toEqual(ids);
    expect(h.edges).toHaveLength(2);
  });

  test('does not add late text nodes after the owning run is canceled', async () => {
    const h = harness();
    let allowCommit = true;
    let release!: (content: string | null) => void;
    const loaded = new Promise<string | null>((resolve) => {
      release = resolve;
    });

    const spawning = spawnResultNodes(h.rf, h.card, 'text', [asset('late')], {
      loadText: async () => loaded,
      shouldCommit: () => allowCommit,
    });
    allowCommit = false;
    release('late result');
    await spawning;

    expect(h.nodes).toHaveLength(0);
    expect(h.edges).toHaveLength(0);
  });

  test('keeps every result addressable in-card while moving the primary first', () => {
    const ids = [asset('first'), asset('primary'), asset('last')];
    expect(orderResultAssetIds(ids, ids[1])).toEqual([ids[1], ids[0], ids[2]]);
    expect(orderResultAssetIds(ids)).toEqual(ids);
  });
});
