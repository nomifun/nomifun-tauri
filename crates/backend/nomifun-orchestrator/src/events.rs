//! Realtime WebSocket event emitter for the 「智能编排」(orchestration) Run engine.
//!
//! [`OrchestratorRunEventEmitter`] is the thin seam the Run engine (Task 6) calls
//! to stream run/task lifecycle status to connected frontends. It mirrors the
//! [`nomifun_cron::CronEventEmitter`] pattern exactly: hold an
//! `Arc<dyn EventBroadcaster>`, build a `serde_json::json!({…})` payload per
//! event, and broadcast a `WebSocketMessage::new("<wire.name>", payload)`.
//!
//! Event names (wire contract — mirrored hand-written in
//! `ui/src/common/types/orchestrator/orchestratorEvents.ts`):
//! - `orchestrator.run.statusChanged`  → `{ run_id, status }`
//! - `orchestrator.run.planUpdated`    → `{ run_id }`
//! - `orchestrator.task.statusChanged` → `{ run_id, task_id, status }`
//! - `orchestrator.task.assigned`      → `{ run_id, task_id, member_id }`
//! - `orchestrator.run.completed`      → `{ run_id, status }`
//! - `orchestrator.run.leadThinking`   → `{ run_id, phase, kind, delta?, content?, done }`

use std::sync::Arc;

use nomifun_api_types::WebSocketMessage;
use nomifun_realtime::EventBroadcaster;
use serde_json::json;

/// Emits realtime run/task lifecycle events over the WebSocket broadcast bus.
#[derive(Clone)]
pub struct OrchestratorRunEventEmitter {
    bus: Arc<dyn EventBroadcaster>,
}

impl OrchestratorRunEventEmitter {
    pub fn new(bus: Arc<dyn EventBroadcaster>) -> Self {
        Self { bus }
    }

    /// A run's overall status changed (e.g. `queued` → `running` → `failed`).
    pub fn emit_run_status(&self, run_id: &str, status: &str) {
        self.bus.broadcast(WebSocketMessage::new(
            "orchestrator.run.statusChanged",
            json!({ "run_id": run_id, "status": status }),
        ));
    }

    /// A run's plan (tasks / dependencies) was (re)produced or revised.
    pub fn emit_run_plan_updated(&self, run_id: &str) {
        self.bus.broadcast(WebSocketMessage::new(
            "orchestrator.run.planUpdated",
            json!({ "run_id": run_id }),
        ));
    }

    /// A single task's status changed (e.g. `pending` → `running` → `done`).
    pub fn emit_task_status(&self, run_id: &str, task_id: &str, status: &str) {
        self.bus.broadcast(WebSocketMessage::new(
            "orchestrator.task.statusChanged",
            json!({ "run_id": run_id, "task_id": task_id, "status": status }),
        ));
    }

    /// A task was assigned to a fleet member (worker).
    pub fn emit_task_assigned(&self, run_id: &str, task_id: &str, member_id: &str) {
        self.bus.broadcast(WebSocketMessage::new(
            "orchestrator.task.assigned",
            json!({ "run_id": run_id, "task_id": task_id, "member_id": member_id }),
        ));
    }

    /// A run reached a terminal state (`completed` / `failed` / `cancelled`).
    pub fn emit_run_completed(&self, run_id: &str, status: &str) {
        self.bus.broadcast(WebSocketMessage::new(
            "orchestrator.run.completed",
            json!({ "run_id": run_id, "status": status }),
        ));
    }

    /// The lead (主) agent's planning thought stream — incremental reasoning /
    /// draft text or a phase-narration key — fanned out so the frontend can
    /// render the live 编排思考 bubble. `phase` ∈ `plan|adjust|summarize`,
    /// `kind` ∈ `reasoning|text|phase`. `delta`/`content` are optional and
    /// omitted from the payload when `None`.
    pub fn emit_lead_thinking(
        &self,
        run_id: &str,
        phase: &str,
        kind: &str,
        delta: Option<&str>,
        content: Option<&str>,
        done: bool,
    ) {
        let mut payload = json!({
            "run_id": run_id,
            "phase": phase,
            "kind": kind,
            "done": done,
        });
        if let Some(delta) = delta {
            payload["delta"] = json!(delta);
        }
        if let Some(content) = content {
            payload["content"] = json!(content);
        }
        self.bus.broadcast(WebSocketMessage::new(
            "orchestrator.run.leadThinking",
            payload,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock broadcaster capturing every broadcast [`WebSocketMessage`] for assertions.
    struct RecordingBroadcaster {
        events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(vec![]),
            }
        }

        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_emitter() -> (OrchestratorRunEventEmitter, Arc<RecordingBroadcaster>) {
        let bc = Arc::new(RecordingBroadcaster::new());
        let emitter = OrchestratorRunEventEmitter::new(bc.clone());
        (emitter, bc)
    }

    #[test]
    fn task_status_event_shape() {
        let (emitter, bc) = make_emitter();
        emitter.emit_task_status("run_1", "rtask_1", "running");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.task.statusChanged");
        assert_eq!(events[0].data["run_id"], "run_1");
        assert_eq!(events[0].data["task_id"], "rtask_1");
        assert_eq!(events[0].data["status"], "running");
    }

    #[test]
    fn run_status_event_shape() {
        let (emitter, bc) = make_emitter();
        emitter.emit_run_status("run_1", "running");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.run.statusChanged");
        assert_eq!(events[0].data["run_id"], "run_1");
        assert_eq!(events[0].data["status"], "running");
    }

    #[test]
    fn run_plan_updated_event_shape() {
        let (emitter, bc) = make_emitter();
        emitter.emit_run_plan_updated("run_1");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.run.planUpdated");
        assert_eq!(events[0].data["run_id"], "run_1");
    }

    #[test]
    fn task_assigned_event_shape() {
        let (emitter, bc) = make_emitter();
        emitter.emit_task_assigned("run_1", "rtask_1", "fmem_7");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.task.assigned");
        assert_eq!(events[0].data["run_id"], "run_1");
        assert_eq!(events[0].data["task_id"], "rtask_1");
        assert_eq!(events[0].data["member_id"], "fmem_7");
    }

    #[test]
    fn run_completed_event_shape() {
        let (emitter, bc) = make_emitter();
        emitter.emit_run_completed("run_1", "completed");

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.run.completed");
        assert_eq!(events[0].data["run_id"], "run_1");
        assert_eq!(events[0].data["status"], "completed");
    }

    #[test]
    fn lead_thinking_event_shape_with_delta() {
        let (emitter, bc) = make_emitter();
        emitter.emit_lead_thinking("run_1", "plan", "reasoning", Some("step "), None, false);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.run.leadThinking");
        assert_eq!(events[0].data["run_id"], "run_1");
        assert_eq!(events[0].data["phase"], "plan");
        assert_eq!(events[0].data["kind"], "reasoning");
        assert_eq!(events[0].data["delta"], "step ");
        assert_eq!(events[0].data["done"], false);
        // content is None → field omitted entirely
        assert!(events[0].data.get("content").is_none());
    }

    #[test]
    fn lead_thinking_event_shape_with_content() {
        let (emitter, bc) = make_emitter();
        emitter.emit_lead_thinking("run_2", "summarize", "text", None, Some("final"), true);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.run.leadThinking");
        assert_eq!(events[0].data["run_id"], "run_2");
        assert_eq!(events[0].data["phase"], "summarize");
        assert_eq!(events[0].data["kind"], "text");
        assert_eq!(events[0].data["content"], "final");
        assert_eq!(events[0].data["done"], true);
        // delta is None → field omitted entirely
        assert!(events[0].data.get("delta").is_none());
    }

    #[test]
    fn lead_thinking_event_phase_omits_both_optionals() {
        let (emitter, bc) = make_emitter();
        emitter.emit_lead_thinking("run_3", "plan", "phase", None, None, false);

        let events = bc.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "orchestrator.run.leadThinking");
        assert_eq!(events[0].data["run_id"], "run_3");
        assert_eq!(events[0].data["phase"], "plan");
        assert_eq!(events[0].data["kind"], "phase");
        assert_eq!(events[0].data["done"], false);
        assert!(events[0].data.get("delta").is_none());
        assert!(events[0].data.get("content").is_none());
    }

    #[test]
    fn multiple_events_accumulate_in_order() {
        let (emitter, bc) = make_emitter();
        emitter.emit_run_status("run_1", "running");
        emitter.emit_task_assigned("run_1", "rtask_1", "fmem_1");
        emitter.emit_task_status("run_1", "rtask_1", "done");
        emitter.emit_run_completed("run_1", "completed");

        let events = bc.events();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].name, "orchestrator.run.statusChanged");
        assert_eq!(events[1].name, "orchestrator.task.assigned");
        assert_eq!(events[2].name, "orchestrator.task.statusChanged");
        assert_eq!(events[3].name, "orchestrator.run.completed");
    }
}
