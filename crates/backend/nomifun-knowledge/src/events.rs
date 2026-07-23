//! Installation-owner-scoped WS events for the knowledge domain.

use std::sync::Arc;

use nomifun_api_types::WebSocketMessage;
use nomifun_common::KnowledgeBaseId;
use nomifun_realtime::UserEventSink;

#[derive(Clone)]
pub struct KnowledgeEventEmitter {
    sink: Arc<dyn UserEventSink>,
    authoritative_user_id: Arc<str>,
}

impl KnowledgeEventEmitter {
    pub fn new(
        sink: Arc<dyn UserEventSink>,
        authoritative_user_id: Arc<str>,
    ) -> Self {
        Self {
            sink,
            authoritative_user_id,
        }
    }

    fn broadcast<T: serde::Serialize>(&self, event_name: &str, payload: &T) {
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, event_name, "failed to serialize knowledge event");
                return;
            }
        };
        self.sink.send_to_user(
            &self.authoritative_user_id,
            WebSocketMessage::new(event_name, value),
        );
    }

    pub fn emit_base_created<T: serde::Serialize>(&self, base: &T) {
        self.broadcast("knowledge.base-created", base);
    }

    pub fn emit_base_updated<T: serde::Serialize>(&self, base: &T) {
        self.broadcast("knowledge.base-updated", base);
    }

    pub fn emit_base_deleted(&self, id: &KnowledgeBaseId) {
        self.broadcast(
            "knowledge.base-deleted",
            &serde_json::json!({ "knowledge_base_id": id }),
        );
    }

    pub fn emit_binding_changed<T: serde::Serialize>(&self, binding: &T) {
        self.broadcast("knowledge.binding-changed", binding);
    }

    /// A tag was created / renamed / recolored / reordered / deleted. Consumers
    /// (the filter bar, tag→label maps, the management modal) just re-list, so
    /// the payload is a bare signal rather than a per-entity diff.
    pub fn emit_tag_changed(&self) {
        self.broadcast("knowledge.tag-changed", &serde_json::json!({}));
    }
}
