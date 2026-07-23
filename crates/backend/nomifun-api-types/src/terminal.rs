//! HTTP request/response DTOs for terminal sessions.
//!
//! Terminal sessions are a separate entity from conversations: each one is a
//! PTY-backed process the user interacts with directly. Byte payloads
//! (input/output) are base64-encoded so they survive the JSON
//! `WebSocketMessage<T>` envelope and HTTP bodies without UTF-8 assumptions.

use std::collections::HashMap;

use nomifun_common::{KnowledgeBaseId, TerminalId, TimestampMs};
use serde::{Deserialize, Serialize};

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

/// A terminal session as returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerminalSessionResponse {
    pub terminal_id: TerminalId,
    pub name: String,
    pub cwd: String,
    /// Derived on every read —never persisted: whether `cwd` equals or sits
    /// under the backend-managed default work dir. Mirrors conversations'
    /// `is_temporary_workspace` so the frontend session list can group these
    /// terminals under the default-workpath node.
    #[serde(default)]
    pub is_default_workpath: bool,
    /// Launch program (e.g. "claude", or the `$SHELL` sentinel for a login shell).
    pub command: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    /// "running" | "exited" | "error".
    pub last_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<TimestampMs>,
    /// Base64 of the recent scrollback buffer. Populated only on single-session
    /// GET (for reconnect); omitted from list responses and events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_b64: Option<String>,
}

/// Request to create + spawn a terminal session.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateTerminalRequest {
    #[serde(default)]
    pub name: Option<String>,
    pub cwd: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Defer spawning the PTY until the first `resize` (which carries the real
    /// fitted terminal size). Set by the interactive frontend so a full-screen
    /// TUI (claude) draws at the correct size from its first frame instead of at
    /// the 80×24 default and then jumping —the cause of "garbled until you
    /// resize". Headless callers (cron/AutoWork/gateway) leave this false and
    /// spawn immediately.
    #[serde(default)]
    pub defer_spawn: bool,
    /// Knowledge bases to bind to this terminal at creation (`kind="terminal"`
    /// binding). They are mounted into `{cwd}/.nomi/knowledge/` before the PTY
    /// spawns; best-effort —mount failures never block the launch.
    #[serde(default)]
    pub knowledge_base_ids: Option<Vec<KnowledgeBaseId>>,
}

/// Write bytes (base64) to a terminal's PTY.
#[derive(Debug, Clone, Deserialize)]
pub struct TerminalInputRequest {
    pub data_b64: String,
}

/// Rename and/or (un)pin a terminal session.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateTerminalRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub pinned: Option<bool>,
}

/// Resize a terminal's PTY.
#[derive(Debug, Clone, Deserialize)]
pub struct TerminalResizeRequest {
    pub cols: u16,
    pub rows: u16,
}

/// `terminal.output` WebSocket event payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerminalOutputEvent {
    pub terminal_id: TerminalId,
    pub data_b64: String,
}

/// `terminal.exit` WebSocket event payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerminalExitEvent {
    pub terminal_id: TerminalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// `terminal.removed` WebSocket event payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerminalRemovedPayload {
    pub terminal_id: TerminalId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_request_defaults_cols_rows() {
        let raw = json!({"cwd": "/tmp", "command": "bash"});
        let req: CreateTerminalRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.cols, 80);
        assert_eq!(req.rows, 24);
        assert!(req.args.is_empty());
        assert!(req.env.is_none());
    }

    #[test]
    fn create_request_full() {
        let raw = json!({
            "name": "claude",
            "cwd": "/work",
            "command": "claude",
            "args": ["--dangerously-skip-permissions"],
            "env": {"FOO": "bar"},
            "backend": "claude",
            "mode": "full-auto",
            "cols": 120,
            "rows": 40
        });
        let req: CreateTerminalRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("claude"));
        assert_eq!(req.args, vec!["--dangerously-skip-permissions"]);
        assert_eq!(req.cols, 120);
        assert_eq!(req.backend.as_deref(), Some("claude"));
        assert_eq!(req.env.unwrap().get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn create_request_knowledge_base_ids_default_and_parse() {
        // Absent → None (older clients keep working).
        let raw = json!({"cwd": "/tmp", "command": "bash"});
        let req: CreateTerminalRequest = serde_json::from_value(raw).unwrap();
        assert!(req.knowledge_base_ids.is_none());

        // Present → bound at creation by the terminal service.
        let kb_1 = nomifun_common::KnowledgeBaseId::new();
        let kb_2 = nomifun_common::KnowledgeBaseId::new();
        let raw = json!({"cwd": "/tmp", "command": "bash", "knowledge_base_ids": [kb_1, kb_2]});
        let req: CreateTerminalRequest = serde_json::from_value(raw).unwrap();
        let parsed = req.knowledge_base_ids.unwrap();
        assert_eq!(
            parsed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![kb_1.to_string(), kb_2.to_string()]
        );
    }

    #[test]
    fn create_request_rejects_noncanonical_knowledge_base_ids() {
        for invalid in [
            json!(42),
            json!("kb_1"),
            json!(""),
            json!("knowledge_base_0190f5fe-7c00-7a00-8abc-012345678901"),
        ] {
            let raw = json!({
                "cwd": "/tmp",
                "command": "bash",
                "knowledge_base_ids": [invalid]
            });
            assert!(serde_json::from_value::<CreateTerminalRequest>(raw).is_err());
        }
    }

    #[test]
    fn terminal_wire_entities_reject_noncanonical_ids() {
        let response = json!({
            "terminal_id": "term_1",
            "name": "shell",
            "cwd": "/tmp",
            "command": "$SHELL",
            "args": [],
            "cols": 80,
            "rows": 24,
            "created_at": 1,
            "updated_at": 1,
            "last_status": "running"
        });
        assert!(serde_json::from_value::<TerminalSessionResponse>(response).is_err());
        assert!(serde_json::from_value::<TerminalOutputEvent>(json!({
            "terminal_id": 42,
            "data_b64": ""
        })).is_err());
        assert!(serde_json::from_value::<TerminalRemovedPayload>(json!({
            "terminal_id": "term_0190f5fe-7c00-7a00-8000-000000000001"
        })).is_err());
    }

    #[test]
    fn terminal_wire_entities_reject_legacy_id_field() {
        let terminal_id = nomifun_common::TerminalId::new();
        let legacy_id = nomifun_common::TerminalId::new();
        let response = json!({
            "terminal_id": terminal_id,
            "id": legacy_id,
            "name": "shell",
            "cwd": "/tmp",
            "command": "$SHELL",
            "args": [],
            "cols": 80,
            "rows": 24,
            "created_at": 1,
            "updated_at": 1,
            "last_status": "running"
        });
        assert!(serde_json::from_value::<TerminalSessionResponse>(response).is_err());
        assert!(serde_json::from_value::<TerminalOutputEvent>(json!({
            "terminal_id": terminal_id,
            "id": legacy_id,
            "data_b64": ""
        })).is_err());
        assert!(serde_json::from_value::<TerminalExitEvent>(json!({
            "terminal_id": terminal_id,
            "id": legacy_id
        })).is_err());
        assert!(serde_json::from_value::<TerminalRemovedPayload>(json!({
            "terminal_id": terminal_id,
            "id": legacy_id
        })).is_err());
    }

    #[test]
    fn create_request_missing_cwd_fails() {
        let raw = json!({"command": "bash"});
        assert!(serde_json::from_value::<CreateTerminalRequest>(raw).is_err());
    }

    #[test]
    fn create_request_missing_command_fails() {
        let raw = json!({"cwd": "/tmp"});
        assert!(serde_json::from_value::<CreateTerminalRequest>(raw).is_err());
    }

    #[test]
    fn session_response_roundtrip_and_omits_none() {
        let resp = TerminalSessionResponse {
            terminal_id: nomifun_common::TerminalId::new(),
            name: "shell".into(),
            cwd: "/tmp".into(),
            is_default_workpath: false,
            command: "$SHELL".into(),
            args: vec![],
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            created_at: 1000,
            updated_at: 2000,
            last_status: "running".into(),
            exit_code: None,
            pinned: false,
            pinned_at: None,
            scrollback_b64: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert!(v.get("backend").is_none());
        assert!(v.get("exit_code").is_none());
        assert!(v.get("scrollback_b64").is_none());
        assert!(v.get("id").is_none());
        assert_eq!(
            v.get("terminal_id").and_then(serde_json::Value::as_str),
            Some(resp.terminal_id.as_str())
        );
        let parsed: TerminalSessionResponse = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn input_request_deserialize() {
        let raw = json!({"data_b64": "aGk="});
        let req: TerminalInputRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.data_b64, "aGk=");
    }

    #[test]
    fn resize_request_deserialize() {
        let raw = json!({"cols": 100, "rows": 30});
        let req: TerminalResizeRequest = serde_json::from_value(raw).unwrap();
        assert_eq!((req.cols, req.rows), (100, 30));
    }

    #[test]
    fn output_event_roundtrip() {
        let e = TerminalOutputEvent {
            terminal_id: nomifun_common::TerminalId::new(),
            data_b64: "ZGF0YQ==".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"terminal_id\""));
        assert!(!s.contains("\"id\""));
        assert_eq!(serde_json::from_str::<TerminalOutputEvent>(&s).unwrap(), e);
    }

    #[test]
    fn exit_event_omits_none_code() {
        let e = TerminalExitEvent {
            terminal_id: nomifun_common::TerminalId::new(),
            exit_code: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("id").is_none());
        assert!(v.get("exit_code").is_none());
    }
}
