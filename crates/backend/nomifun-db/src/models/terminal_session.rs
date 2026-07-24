use nomifun_common::{TerminalId, TimestampMs, UserId};
use serde::{Deserialize, Serialize};

/// Database row for the `terminal_sessions` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TerminalSessionRow {
    pub id: i64,
    #[sqlx(try_from = "String")]
    pub terminal_id: TerminalId,
    pub name: String,
    pub cwd: String,
    pub command: String,
    /// JSON array of args.
    pub args: String,
    /// JSON object of env vars, nullable.
    pub env: Option<String>,
    pub backend: Option<String>,
    pub mode: Option<String>,
    pub cols: i64,
    pub rows: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    /// "running" | "exited" | "error".
    pub last_status: String,
    pub exit_code: Option<i64>,
    #[sqlx(try_from = "String")]
    pub user_id: UserId,
    pub pinned: bool,
    pub pinned_at: Option<TimestampMs>,
    /// AutoWork config JSON `{enabled, tag, max_requirements}`, nullable. Drives
    /// the Requirements Platform AutoWork execution loop for this terminal.
    pub autowork: Option<String>,
    /// IDMM config JSON, nullable. When set, the terminal operates under
    /// Iterative-Deepening Mental-Model guidance.
    pub idmm: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_session_row_roundtrip() {
        let row = TerminalSessionRow {
            id: 1,
            terminal_id: nomifun_common::TerminalId::new(),
            name: "claude".into(),
            cwd: "/work".into(),
            command: "claude".into(),
            args: r#"["--dangerously-skip-permissions"]"#.into(),
            env: Some(r#"{"FOO":"bar"}"#.into()),
            backend: Some("claude".into()),
            mode: Some("full-auto".into()),
            cols: 120,
            rows: 40,
            created_at: 1000,
            updated_at: 2000,
            last_status: "running".into(),
            exit_code: None,
            user_id: nomifun_common::UserId::new(),
            pinned: false,
            pinned_at: None,
            autowork: None,
            idmm: None,
        };
        let expected_id = row.terminal_id.clone();
        let json = serde_json::to_string(&row).unwrap();
        let restored: TerminalSessionRow = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.terminal_id, expected_id);
        assert_eq!(restored.cols, 120);
        assert_eq!(restored.last_status, "running");
        assert!(restored.exit_code.is_none());
    }

    #[test]
    fn terminal_session_row_optional_none() {
        let row = TerminalSessionRow {
            id: 1,
            terminal_id: nomifun_common::TerminalId::new(),
            name: "shell".into(),
            cwd: "/tmp".into(),
            command: "$SHELL".into(),
            args: "[]".into(),
            env: None,
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            created_at: 1,
            updated_at: 1,
            last_status: "exited".into(),
            exit_code: Some(0),
            user_id: nomifun_common::UserId::new(),
            pinned: true,
            pinned_at: Some(123),
            autowork: Some(r#"{"enabled":true,"tag":"t"}"#.into()),
            idmm: None,
        };
        assert!(row.env.is_none());
        assert!(row.backend.is_none());
        assert_eq!(row.exit_code, Some(0));
        assert!(row.pinned);
        assert!(row.autowork.is_some());
    }
}
