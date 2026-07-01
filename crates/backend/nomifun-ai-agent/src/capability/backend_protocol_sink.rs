use std::sync::{Arc, RwLock};

use nomi_protocol::events::{ProtocolEvent, ToolCategory};
use nomi_protocol::writer::ProtocolEmitter;
use nomifun_common::{Confirmation, ConfirmationOption, generate_id};
use serde_json::json;
use tokio::sync::broadcast;
use tracing::debug;

use crate::protocol::events::{AcpPermissionEventData, AgentStreamEvent, ToolCallEventData, ToolCallStatus};

/// Implements `ProtocolEmitter` for the nomicore context.
///
/// Bridges nomi `ProtocolEvent` emissions to `AgentStreamEvent` on a
/// broadcast channel. Only handles events relevant to the approval flow;
/// other events (text, thinking, tool results) are already handled by
/// `BackendOutputSink` via the `OutputSink` trait.
pub struct BackendProtocolSink {
    event_tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Arc<RwLock<Vec<Confirmation>>>,
}

impl BackendProtocolSink {
    pub fn new(event_tx: broadcast::Sender<AgentStreamEvent>, confirmations: Arc<RwLock<Vec<Confirmation>>>) -> Self {
        Self {
            event_tx,
            confirmations,
        }
    }

    fn build_confirmation(call_id: &str, tool_name: &str, category: &ToolCategory, description: &str) -> Confirmation {
        let title = format!("{} wants to use: {}", category, tool_name);
        let command_type = Some(category.to_string());

        Confirmation {
            id: generate_id(),
            call_id: call_id.to_string(),
            title: Some(title),
            action: Some(tool_name.to_string()),
            description: description.to_string(),
            command_type,
            options: vec![
                ConfirmationOption {
                    label: "messages.confirmation.yesAllowOnce".to_string(),
                    value: json!("proceed_once"),
                    params: None,
                },
                ConfirmationOption {
                    label: "messages.confirmation.yesAllowAlways".to_string(),
                    value: json!("proceed_always"),
                    params: None,
                },
                ConfirmationOption {
                    label: "messages.confirmation.no".to_string(),
                    value: json!("cancel"),
                    params: None,
                },
            ],
            // Non-browser tool confirmation → no page preview (browser takeover only).
            screenshot: None,
        }
    }
}

impl ProtocolEmitter for BackendProtocolSink {
    fn emit(&self, event: &ProtocolEvent) -> std::io::Result<()> {
        match event {
            ProtocolEvent::ToolRequest { call_id, tool, .. } => {
                let confirmation = Self::build_confirmation(call_id, &tool.name, &tool.category, &tool.description);

                if let Ok(mut confs) = self.confirmations.write() {
                    confs.push(confirmation.clone());
                }

                let _ = self
                    .event_tx
                    .send(AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(
                        confirmation.clone(),
                    )));

                debug!(
                    call_id,
                    tool_name = %tool.name,
                    "BackendProtocolSink: emitted AcpPermission(Confirmation) event"
                );
            }

            ProtocolEvent::ToolCancelled { call_id, reason, .. } => {
                if let Ok(mut confs) = self.confirmations.write() {
                    confs.retain(|c| c.call_id != *call_id);
                }

                let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                    call_id: call_id.clone(),
                    name: format!("cancelled: {reason}"),
                    args: serde_json::Value::Null,
                    status: ToolCallStatus::Error,
                    input: None,
                    output: None,
                    description: None,
                }));
            }

            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_protocol::events::ToolInfo;

    fn make_sink() -> (
        BackendProtocolSink,
        broadcast::Receiver<AgentStreamEvent>,
        Arc<RwLock<Vec<Confirmation>>>,
    ) {
        let (tx, rx) = broadcast::channel(16);
        let confs = Arc::new(RwLock::new(Vec::new()));
        let sink = BackendProtocolSink::new(tx, confs.clone());
        (sink, rx, confs)
    }

    #[test]
    fn tool_request_emits_permission_event() {
        let (sink, mut rx, confs) = make_sink();
        let event = ProtocolEvent::ToolRequest {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            tool: ToolInfo {
                name: "Write".into(),
                category: ToolCategory::Edit,
                args: json!({"path": "/tmp/test.txt"}),
                description: "Write file /tmp/test.txt".into(),
            },
        };

        sink.emit(&event).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(conf)) => {
                assert_eq!(conf.call_id, "c1");
                assert!(conf.options.len() >= 3);
            }
            other => panic!("Expected AcpPermission(Confirmation), got {:?}", other),
        }

        let stored = confs.read().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].call_id, "c1");
    }

    #[test]
    fn tool_running_is_ignored() {
        let (sink, mut rx, _) = make_sink();
        let event = ProtocolEvent::ToolRunning {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            tool_name: "Write".into(),
        };

        sink.emit(&event).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tool_cancelled_removes_confirmation_and_emits_error() {
        let (sink, mut rx, confs) = make_sink();

        let req = ProtocolEvent::ToolRequest {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            tool: ToolInfo {
                name: "Bash".into(),
                category: ToolCategory::Exec,
                args: json!({"command": "rm -rf /"}),
                description: "Execute: rm -rf /".into(),
            },
        };
        sink.emit(&req).unwrap();
        let _ = rx.try_recv().unwrap();

        assert_eq!(confs.read().unwrap().len(), 1);

        let cancel = ProtocolEvent::ToolCancelled {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            reason: "User denied".into(),
        };
        sink.emit(&cancel).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "c1");
                assert_eq!(data.status, ToolCallStatus::Error);
            }
            other => panic!("Expected ToolCall error, got {:?}", other),
        }

        assert_eq!(confs.read().unwrap().len(), 0);
    }

    #[test]
    fn other_events_are_ignored() {
        let (sink, mut rx, _) = make_sink();
        let event = ProtocolEvent::StreamStart { msg_id: "m1".into() };

        sink.emit(&event).unwrap();

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn no_panic_when_no_receivers() {
        let (tx, _) = broadcast::channel(16);
        let confs = Arc::new(RwLock::new(Vec::new()));
        let sink = BackendProtocolSink::new(tx, confs);
        let event = ProtocolEvent::ToolRequest {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            tool: ToolInfo {
                name: "Read".into(),
                category: ToolCategory::Info,
                args: json!({}),
                description: "Read file".into(),
            },
        };
        sink.emit(&event).unwrap();
    }

    #[test]
    fn confirmation_has_three_options() {
        let conf =
            BackendProtocolSink::build_confirmation("c1", "Write", &ToolCategory::Edit, "Write file /tmp/test.txt");
        assert_eq!(conf.options.len(), 3);
        assert_eq!(conf.options[0].value, json!("proceed_once"));
        assert_eq!(conf.options[1].value, json!("proceed_always"));
        assert_eq!(conf.options[2].value, json!("cancel"));
    }
}
