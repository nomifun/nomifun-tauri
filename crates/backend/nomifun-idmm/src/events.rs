//! WebSocket event emission for IDMM. Mirrors `nomifun_requirement::events`.
//! Event names follow the `domain.camelCaseAction` convention.

use std::sync::Arc;

use nomifun_api_types::{IdmmState, InterventionRecord, WebSocketMessage};
use nomifun_realtime::UserEventSink;
use tracing::error;

/// Emits IDMM status + intervention events through the shared broadcaster.
#[derive(Clone)]
pub struct IdmmEventEmitter {
    user_events: Arc<dyn UserEventSink>,
}

impl IdmmEventEmitter {
    pub fn new(user_events: Arc<dyn UserEventSink>) -> Self {
        Self { user_events }
    }

    /// `idmm.statusChanged` — armed/disabled/intervening transitions.
    pub fn emit_status_changed(&self, owner_id: &str, state: &IdmmState) {
        self.send(owner_id, "idmm.statusChanged", state);
    }

    /// `idmm.intervention` — one intervention happened (detected → action → outcome).
    pub fn emit_intervention(&self, owner_id: &str, record: &InterventionRecord) {
        self.send(owner_id, "idmm.intervention", record);
    }

    fn send<T: serde::Serialize>(&self, owner_id: &str, event_name: &str, payload: &T) {
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                error!(event = event_name, error = %e, "IDMM event serialize failed");
                return;
            }
        };
        self.user_events
            .send_to_user(owner_id, WebSocketMessage::new(event_name, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::{IdmmRunState, IdmmTargetKind};
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturingBroadcaster {
        events: Mutex<Vec<(String, WebSocketMessage<serde_json::Value>)>>,
    }
    impl UserEventSink for CapturingBroadcaster {
        fn send_to_user(&self, user_id: &str, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push((user_id.to_owned(), event));
        }
    }

    #[test]
    fn emits_status_changed_with_payload() {
        let bc = Arc::new(CapturingBroadcaster::default());
        let emitter = IdmmEventEmitter::new(bc.clone());
        let st = IdmmState {
            kind: IdmmTargetKind::Conversation,
            target_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
            enabled: true,
            fault_enabled: false,
            decision_enabled: true,
            run_state: IdmmRunState::Armed,
            interventions_count: 0,
            last_signal: None,
            last_intervention_at: None,
            sidecar_provider_resolved: false,
            config: None,
        };
        emitter.emit_status_changed("0190f5fe-7c00-7a00-8000-000000000001", &st);
        let evs = bc.events.lock().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].0, "0190f5fe-7c00-7a00-8000-000000000001");
        assert_eq!(evs[0].1.name, "idmm.statusChanged");
        assert_eq!(
            evs[0].1.data["target_id"],
            "0190f5fe-7c00-7a00-8000-000000000001"
        );
        assert_eq!(evs[0].1.data["run_state"], "armed");
    }

    #[test]
    fn emits_intervention_with_payload() {
        let bc = Arc::new(CapturingBroadcaster::default());
        let emitter = IdmmEventEmitter::new(bc.clone());
        let intervention_id = nomifun_common::IdmmInterventionId::new();
        let rec = InterventionRecord {
            intervention_id: intervention_id.clone(),
            target_kind: "conversation".into(),
            target_id: "0190f5fe-7c00-7a00-8000-000000000001".into(),
            watch: "fault".into(),
            at: 123,
            stall_class: "provider_error".into(),
            tier_used: "rule".into(),
            category: None,
            action: "retry".into(),
            detail: None,
            outcome: "applied".into(),
            reason: Some("transient 500".into()),
            confidence: None,
            bypass_model: None,
        };
        emitter.emit_intervention("0190f5fe-7c00-7a00-8000-000000000002", &rec);
        let evs = bc.events.lock().unwrap();
        assert_eq!(evs[0].0, "0190f5fe-7c00-7a00-8000-000000000002");
        assert_eq!(evs[0].1.name, "idmm.intervention");
        assert_eq!(evs[0].1.data["intervention_id"], intervention_id.as_str());
        assert!(evs[0].1.data.get("id").is_none());
        assert_eq!(evs[0].1.data["action"], "retry");
    }
}
