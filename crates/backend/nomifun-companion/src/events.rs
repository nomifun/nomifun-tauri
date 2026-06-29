//! WS push events for the companion domain. Same shape as `CronEventEmitter`:
//! a thin wrapper over the global `EventBroadcaster`.

use std::sync::Arc;

use nomifun_api_types::WebSocketMessage;
use nomifun_realtime::EventBroadcaster;

#[derive(Clone)]
pub struct CompanionEventEmitter {
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl CompanionEventEmitter {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { broadcaster }
    }

    fn broadcast<T: serde::Serialize>(&self, event_name: &str, payload: &T) {
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, event_name, "failed to serialize companion event");
                return;
            }
        };
        self.broadcaster.broadcast(WebSocketMessage::new(event_name, value));
    }

    /// 把 `companion_id` 合并进结构体序列化出的对象顶层后广播。用于 learn/evolve
    /// 产出的"应由单个伙伴呈现"的事件（沿用 emit_companion_updated 的对象合并手法）。
    fn broadcast_scoped<T: serde::Serialize>(&self, event_name: &str, companion_id: &str, payload: &T) {
        let mut map = match serde_json::to_value(payload) {
            Ok(serde_json::Value::Object(map)) => map,
            Ok(other) => {
                let mut m = serde_json::Map::new();
                m.insert("value".into(), other);
                m
            }
            Err(e) => {
                tracing::warn!(error = %e, event_name, "failed to serialize scoped companion event");
                return;
            }
        };
        map.insert("companion_id".into(), serde_json::Value::String(companion_id.to_owned()));
        self.broadcaster.broadcast(WebSocketMessage::new(event_name, serde_json::Value::Object(map)));
    }

    pub fn emit_suggestion_created(&self, companion_id: &str, suggestion: &crate::store::CompanionSuggestion) {
        self.broadcast_scoped("companion.suggestion-created", companion_id, suggestion);
    }

    /// A suggestion was accepted/dismissed. Lets every open surface (panel,
    /// desktop bubble, console) drop the now-decided card live instead of
    /// leaving a stale `new` snapshot that 404s on the next decide. Payload is
    /// the decided suggestion (carries `id` + new `status`).
    pub fn emit_suggestion_decided(&self, suggestion: &crate::store::CompanionSuggestion) {
        self.broadcast("companion.suggestion-decided", suggestion);
    }

    pub fn emit_learn_started(&self, companion_id: &str) {
        self.broadcast("companion.learn-started", &serde_json::json!({ "companion_id": companion_id }));
    }

    pub fn emit_learn_finished(&self, companion_id: &str, run: &crate::store::CompanionLearnRun) {
        self.broadcast_scoped("companion.learn-finished", companion_id, run);
    }

    pub fn emit_mood_changed(&self, companion_id: &str, mood: &str) {
        self.broadcast("companion.mood-changed", &serde_json::json!({ "companion_id": companion_id, "mood": mood }));
    }

    /// Shared (cross-companion) config changed. Same event name the legacy single
    /// config used; the payload carries `"scope": "shared"` so listeners can
    /// tell it apart from per-companion profile updates.
    pub fn emit_shared_config_updated(&self, config: &crate::profile::SharedCompanionConfig) {
        let mut payload = match serde_json::to_value(config) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        payload.insert("scope".into(), serde_json::Value::String("shared".into()));
        self.broadcast("companion.config-updated", &serde_json::Value::Object(payload));
    }

    /// One companion's profile changed. `"scope"` is the companion id, the rest of the
    /// payload is the full profile. `"companion_id"` is also set explicitly so listeners
    /// that key off it (useCompanions) don't have to fall back to parsing `scope`.
    pub fn emit_companion_updated(&self, companion_id: &str, profile: &crate::profile::CompanionProfileConfig) {
        let mut payload = match serde_json::to_value(profile) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        payload.insert("scope".into(), serde_json::Value::String(companion_id.to_owned()));
        payload.insert("companion_id".into(), serde_json::Value::String(companion_id.to_owned()));
        self.broadcast("companion.config-updated", &serde_json::Value::Object(payload));
    }

    pub fn emit_companion_created(&self, profile: &crate::profile::CompanionProfileConfig) {
        // Wire shape matches the frontend ICompanionCreatedEvent { companion_id, profile }.
        // The raw profile carries `id` but no `companion_id`, so useCompanions.refreshOne
        // (which reads evt.companion_id) silently no-op'd on the incremental roster add.
        self.broadcast(
            "companion.created",
            &serde_json::json!({ "companion_id": profile.id.clone(), "profile": profile }),
        );
    }

    pub fn emit_companion_deleted(&self, companion_id: &str) {
        self.broadcast("companion.deleted", &serde_json::json!({ "companion_id": companion_id }));
    }

    /// A memory was created outside a learn run (companion-chat save_memory
    /// tool or manual add) — lets open UIs refresh counters live.
    pub fn emit_memory_created(&self, memory: &crate::store::CompanionMemory) {
        self.broadcast("companion.memory-created", memory);
    }

    /// A memory's content/scope/pin/status was edited. Lets every open surface
    /// (memories tab, desktop bubble, second window) reflect the edit live
    /// instead of holding a stale snapshot.
    pub fn emit_memory_updated(&self, memory: &crate::store::CompanionMemory) {
        self.broadcast("companion.memory-updated", memory);
    }

    /// A memory was hard-deleted. Payload carries the `id` so listeners can drop
    /// the row without a refetch.
    pub fn emit_memory_deleted(&self, id: &str) {
        self.broadcast("companion.memory-deleted", &serde_json::json!({ "id": id }));
    }

    /// A skill draft was auto-generated and is awaiting review.
    pub fn emit_skill_drafted(&self, companion_id: &str, skill_name: &str) {
        self.broadcast(
            "companion.skill-drafted",
            &serde_json::json!({ "companion_id": companion_id, "skill_name": skill_name }),
        );
    }

    /// A skill was accepted/activated — the companion just "learned" it.
    pub fn emit_skill_learned(&self, companion_id: &str, skill_name: &str) {
        self.broadcast(
            "companion.skill-learned",
            &serde_json::json!({ "companion_id": companion_id, "skill_name": skill_name }),
        );
    }

    /// A skill was auto-archived by the decay pass (unused too long).
    pub fn emit_skill_archived(&self, companion_id: &str, skill_name: &str) {
        self.broadcast(
            "companion.skill-archived",
            &serde_json::json!({ "companion_id": companion_id, "skill_name": skill_name }),
        );
    }
}
