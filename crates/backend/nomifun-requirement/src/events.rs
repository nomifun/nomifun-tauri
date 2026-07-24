//! Installation-owner-scoped realtime events for requirements and AutoWork.

use std::sync::Arc;

use nomifun_api_types::{AutoWorkState, Requirement, RequirementDeletedPayload, TagPausedPayload, WebSocketMessage};
use nomifun_common::{ConversationId, RequirementId, TerminalId, UserId};
use nomifun_realtime::UserEventSink;
use tracing::error;

/// Emits Requirements-Platform WebSocket events (`domain.camelCaseAction`).
#[derive(Clone)]
pub struct RequirementEventEmitter {
    sink: Arc<dyn UserEventSink>,
    authoritative_user_id: Arc<str>,
}

impl RequirementEventEmitter {
    pub fn new(
        sink: Arc<dyn UserEventSink>,
        authoritative_user_id: Arc<str>,
    ) -> Self {
        Self {
            sink,
            authoritative_user_id,
        }
    }

    pub fn emit_created(&self, req: &Requirement) {
        self.broadcast("requirement.created", req);
    }

    pub fn emit_updated(&self, req: &Requirement) {
        self.broadcast("requirement.updated", req);
    }

    pub fn emit_status_changed(&self, req: &Requirement) {
        self.broadcast("requirement.statusChanged", req);
    }

    pub fn emit_deleted(&self, requirement_id: &str) {
        if RequirementId::parse(requirement_id).is_err() {
            error!(requirement_id, "Refusing to emit a requirement event with an invalid id");
            return;
        }
        self.broadcast(
            "requirement.deleted",
            &RequirementDeletedPayload {
                requirement_id: requirement_id.to_owned(),
            },
        );
    }

    pub fn emit_autowork_changed(&self, state: &AutoWorkState) {
        let target_valid = match state.kind {
            nomifun_api_types::AutoWorkTargetKind::Conversation => {
                ConversationId::try_from(state.target_id.as_str()).is_ok()
            }
            nomifun_api_types::AutoWorkTargetKind::Terminal => {
                TerminalId::try_from(state.target_id.as_str()).is_ok()
            }
        };
        if !target_valid
            || state
                .current_requirement_id
                .as_deref()
                .is_some_and(|id| RequirementId::parse(id).is_err())
        {
            error!(target_id = %state.target_id, "Refusing to emit an AutoWork event with invalid durable ids");
            return;
        }
        self.broadcast("autowork.statusChanged", state);
    }

    /// AutoWork paused a tag after a requirement exhausted its retries.
    pub fn emit_tag_paused(&self, payload: &TagPausedPayload) {
        self.broadcast("autowork.tagPaused", payload);
    }

    fn broadcast<T: serde::Serialize>(&self, event_name: &str, payload: &T) {
        if let Err(error) = UserId::try_from(self.authoritative_user_id.as_ref()) {
            error!(event_name, error = %error, "Refusing to emit a requirement event for an invalid user id");
            return;
        }
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                error!(event_name, error = %e, "Failed to serialize requirement event payload");
                return;
            }
        };
        self.sink.send_to_user(
            &self.authoritative_user_id,
            WebSocketMessage::new(event_name, value),
        );
    }
}
