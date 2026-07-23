use std::sync::Arc;

use nomifun_api_types::{
    TerminalExitEvent, TerminalOutputEvent, TerminalRemovedPayload, TerminalSessionResponse, WebSocketMessage,
};
use nomifun_common::TerminalId;
use nomifun_realtime::UserEventSink;
use tracing::error;

/// Broadcasts terminal lifecycle + stream events over the realtime WebSocket bus.
#[derive(Clone)]
pub struct TerminalEventEmitter {
    user_events: Arc<dyn UserEventSink>,
}

impl TerminalEventEmitter {
    pub fn new(user_events: Arc<dyn UserEventSink>) -> Self {
        Self { user_events }
    }

    /// A chunk of PTY output (base64-encoded bytes).
    pub fn emit_output(&self, owner_id: &str, id: &TerminalId, data_b64: String) {
        self.send(
            owner_id,
            "terminal.output",
            &TerminalOutputEvent {
                terminal_id: id.clone(),
                data_b64,
            },
        );
    }

    /// The child process exited.
    pub fn emit_exit(&self, owner_id: &str, id: &TerminalId, exit_code: Option<i32>) {
        self.send(
            owner_id,
            "terminal.exit",
            &TerminalExitEvent {
                terminal_id: id.clone(),
                exit_code,
            },
        );
    }

    pub fn emit_created(&self, owner_id: &str, session: &TerminalSessionResponse) {
        self.send(owner_id, "terminal.created", session);
    }

    pub fn emit_updated(&self, owner_id: &str, session: &TerminalSessionResponse) {
        self.send(owner_id, "terminal.updated", session);
    }

    pub fn emit_removed(&self, owner_id: &str, id: &TerminalId) {
        self.send(
            owner_id,
            "terminal.removed",
            &TerminalRemovedPayload {
                terminal_id: id.clone(),
            },
        );
    }

    fn send<T: serde::Serialize>(&self, owner_id: &str, event_name: &str, payload: &T) {
        let value = match serde_json::to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                error!(event_name, error = %e, "Failed to serialize terminal event payload");
                return;
            }
        };
        self.user_events
            .send_to_user(owner_id, WebSocketMessage::new(event_name, value));
    }
}
