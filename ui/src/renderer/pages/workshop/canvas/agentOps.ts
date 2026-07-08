/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * 画布助手 (canvas assistant) op applier.
 *
 * While a canvas is open, this hook polls the backend's pending agent-op queue
 * (`GET /api/workshop/canvases/{id}/pending-ops`) and applies each op to the
 * LIVE react-flow graph through handlers the editor supplies — so an agent's
 * writes land through the same code path a user's edits do (history + debounced
 * autosave), never by the backend racing the frontend's whole-doc autosave.
 *
 * The poll itself registers the canvas as "open" on the backend, which is how
 * the workshop service decides to queue ops (frontend authoritative) vs. apply
 * `add_node`/`connect` directly (canvas closed). Applied ops are ACKed so the
 * backend drops them; a client-side applied-set also dedupes across polls so a
 * flaky ACK can't double-apply.
 */

import { useEffect, useRef } from 'react';

import { httpRequest } from '@/common/adapter/httpBridge';

/** Node kinds an agent may create (contract §4 interactive kinds). */
export type AgentNodeKind = 'image' | 'text' | 'video' | 'generator';

/** Create a node (position auto-assigned when omitted). */
export interface AgentAddNodeOp {
  type: 'add_node';
  node: {
    kind: AgentNodeKind;
    x?: number;
    y?: number;
    w?: number;
    h?: number;
    data?: Record<string, unknown>;
  };
}

/** Connect two existing nodes (directed, from → to). */
export interface AgentConnectOp {
  type: 'connect';
  from_node_id: string;
  to_node_id: string;
}

/** Shallow-merge a patch into a node's data. */
export interface AgentUpdateNodeDataOp {
  type: 'update_node_data';
  node_id: string;
  patch: Record<string, unknown>;
}

/** Delete a node (and its incident edges). */
export interface AgentDeleteNodeOp {
  type: 'delete_node';
  node_id: string;
}

export type AgentOp = AgentAddNodeOp | AgentConnectOp | AgentUpdateNodeDataOp | AgentDeleteNodeOp;

/** One queued op the backend hands the frontend to apply, then ACK. */
export interface PendingAgentOp {
  op_id: string;
  op: AgentOp;
}

/** Handlers the editor wires to its react-flow mutation primitives. */
export interface AgentOpHandlers {
  addNode: (op: AgentAddNodeOp) => void;
  connect: (op: AgentConnectOp) => void;
  updateNodeData: (nodeId: string, patch: Record<string, unknown>) => void;
  deleteNode: (nodeId: string) => void;
  /** Called after a batch of ops applies (count = ops applied this tick). */
  onApplied?: (count: number) => void;
}

/** Poll cadence (~2.5 s while healthy). */
const POLL_OK_MS = 2500;
/** Backoff after a failed poll (silent). */
const POLL_BACKOFF_MS = 5000;

async function fetchPendingOps(canvasId: string): Promise<PendingAgentOp[]> {
  const res = await httpRequest<{ ops: PendingAgentOp[] }>(
    'GET',
    `/api/workshop/canvases/${encodeURIComponent(canvasId)}/pending-ops`
  );
  return res?.ops ?? [];
}

async function ackOps(canvasId: string, opIds: string[]): Promise<void> {
  if (opIds.length === 0) return;
  await httpRequest('POST', `/api/workshop/canvases/${encodeURIComponent(canvasId)}/pending-ops/ack`, {
    op_ids: opIds,
  });
}

/**
 * Poll + apply queued agent ops for the open canvas. Handlers are read through a
 * ref, so the polling effect only re-subscribes when `canvasId` changes.
 */
export function useAgentOps(canvasId: string, handlers: AgentOpHandlers): void {
  const handlersRef = useRef(handlers);
  handlersRef.current = handlers;

  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;
    // Ops already applied this session — a flaky ACK must not re-apply them.
    const applied = new Set<string>();

    const applyOne = (pending: PendingAgentOp): boolean => {
      if (applied.has(pending.op_id)) return true; // already applied → still ack
      const h = handlersRef.current;
      const op = pending.op;
      try {
        switch (op.type) {
          case 'add_node':
            h.addNode(op);
            break;
          case 'connect':
            h.connect(op);
            break;
          case 'update_node_data':
            h.updateNodeData(op.node_id, op.patch);
            break;
          case 'delete_node':
            h.deleteNode(op.node_id);
            break;
          default:
            return false;
        }
        applied.add(pending.op_id);
        return true;
      } catch {
        return false;
      }
    };

    const tick = async (): Promise<void> => {
      let delay = POLL_OK_MS;
      try {
        const pending = await fetchPendingOps(canvasId);
        if (!cancelled && pending.length > 0) {
          const acked: string[] = [];
          let freshlyApplied = 0;
          for (const p of pending) {
            const before = applied.has(p.op_id);
            if (applyOne(p)) {
              acked.push(p.op_id);
              if (!before) freshlyApplied += 1;
            }
          }
          if (acked.length > 0) await ackOps(canvasId, acked);
          if (freshlyApplied > 0) handlersRef.current.onApplied?.(freshlyApplied);
        }
      } catch {
        delay = POLL_BACKOFF_MS; // silent backoff
      }
      if (!cancelled) {
        timer = window.setTimeout(() => void tick(), delay);
      }
    };

    // Immediate poll on mount → registers the canvas as "open" right away.
    void tick();

    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [canvasId]);
}
