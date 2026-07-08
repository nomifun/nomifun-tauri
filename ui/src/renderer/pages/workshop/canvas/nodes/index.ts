/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Module-level frozen `nodeTypes` registry. Passing a stable object reference to
 * `<ReactFlow>` avoids the "you passed a new nodeTypes object each render"
 * warning and the associated node remounts (DagCanvas convention).
 */

import type { NodeTypes } from '@xyflow/react';
import ImageNode from './ImageNode';
import TextNode from './TextNode';
import VideoNode from './VideoNode';
import GeneratorNode from './GeneratorNode';
import LoopNode from './LoopNode';
import CompareNode from './CompareNode';
import OutputNode from './OutputNode';
import GroupNode from './GroupNode';

export const WORKSHOP_NODE_TYPES: NodeTypes = {
  image: ImageNode,
  text: TextNode,
  video: VideoNode,
  generator: GeneratorNode,
  loop: LoopNode,
  compare: CompareNode,
  output: OutputNode,
  group: GroupNode,
} as const;
