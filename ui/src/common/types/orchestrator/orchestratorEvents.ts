// src/common/types/orchestrator/orchestratorEvents.ts
// 「智能编排」(orchestration) Run-engine realtime event payloads — hand-written
// mirrors of the WebSocket events emitted by the backend
// `OrchestratorRunEventEmitter` (crates/backend/nomifun-orchestrator/src/events.rs).
//
// These mirror the JSON `data` shape of each `WebSocketMessage`, NOT the full
// envelope. Field names are kept snake_case to match the wire exactly,
// consistent with the rest of the codebase's wire types. IDs are STRINGS.
//
// Wire event names (the `name` field of each WebSocketMessage):
//   orchestrator.run.statusChanged  → TOrchRunStatusEvent
//   orchestrator.run.planUpdated    → TOrchRunPlanUpdatedEvent
//   orchestrator.task.statusChanged → TOrchTaskStatusEvent
//   orchestrator.task.assigned      → TOrchTaskAssignedEvent
//   orchestrator.run.completed      → TOrchRunCompletedEvent
//   orchestrator.run.leadThinking   → TOrchRunLeadThinkingEvent

/** WS `orchestrator.run.statusChanged` — a run's overall status changed. */
export type TOrchRunStatusEvent = {
  run_id: string;
  status: string;
};

/** WS `orchestrator.run.planUpdated` — a run's plan (tasks/deps) was (re)produced. */
export type TOrchRunPlanUpdatedEvent = {
  run_id: string;
};

/** WS `orchestrator.task.statusChanged` — a single task's status changed. */
export type TOrchTaskStatusEvent = {
  run_id: string;
  task_id: string;
  status: string;
};

/** WS `orchestrator.task.assigned` — a task was assigned to a fleet member (worker). */
export type TOrchTaskAssignedEvent = {
  run_id: string;
  task_id: string;
  member_id: string;
};

/** WS `orchestrator.run.completed` — a run reached a terminal state. */
export type TOrchRunCompletedEvent = {
  run_id: string;
  status: string;
};

/** Phase of the lead (主) agent's planning thought stream. */
export type TOrchLeadThinkingPhase = 'plan' | 'adjust' | 'summarize';

/** Kind of a lead-thinking delta: incremental reasoning, draft text, or a phase-narration key. */
export type TOrchLeadThinkingKind = 'reasoning' | 'text' | 'phase';

/**
 * WS `orchestrator.run.leadThinking` — the lead (主) agent's planning thought
 * stream: incremental reasoning / draft text or a phase-narration key, fanned
 * out so the frontend can render the live 编排思考 bubble. `delta` and
 * `content` are optional and omitted from the payload when the backend sends
 * `None` (e.g. `kind:"phase"` carries only a semantic key in `content`).
 */
export type TOrchRunLeadThinkingEvent = {
  run_id: string;
  phase: TOrchLeadThinkingPhase;
  kind: TOrchLeadThinkingKind;
  delta?: string;
  content?: string;
  done: boolean;
};
