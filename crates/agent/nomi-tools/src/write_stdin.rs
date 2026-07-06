//! `write_stdin`: write characters to an existing `exec_command` session and
//! return recent output. With `chars=""` it polls without writing. Ctrl-C is
//! `chars=""`.
//!
//! Shares the same `Arc<ProcessStore>` as `ExecCommandTool`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};

use crate::Tool;
use crate::output_truncation::{TruncationBudget, truncate_middle};
use crate::process_store::{ProcessStore, collect_until_deadline};

/// Ctrl-C (ETX). On a PTY, writing this byte triggers SIGINT in the foreground.
const INTERRUPT: &str = "\u{3}";
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const MIN_EMPTY_YIELD_MS: u64 = 5_000;
const MAX_EMPTY_YIELD_MS: u64 = 300_000;
/// Window after a non-empty write before we start polling, so the process has a
/// moment to react (mirrors codex's post-write sleep).
const POST_WRITE_REACT_MS: u64 = 100;
const OUTPUT_CAP_BYTES: usize = 128 * 1024;

pub struct WriteStdinTool {
    store: Arc<ProcessStore>,
}

impl WriteStdinTool {
    pub fn new(store: Arc<ProcessStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WriteStdinTool {
    fn name(&self) -> &str {
        "write_stdin"
    }

    fn description(&self) -> &str {
        "Writes characters to an existing exec_command session and returns recent output.\n\n\
         - chars defaults to empty, which POLLS for output without writing anything.\n\
         - Send Ctrl-C with chars=\"\\u0003\".\n\
         - To submit a command line to an interactive program, send the line of text in one \
         call, then the Enter/return key (\"\\r\") as a SEPARATE write_stdin call — sending text \
         and the carriage return together can be swallowed by a TUI's paste-burst detection.\n\n\
         If the process has exited, the result reports its exit_code; otherwise it echoes the \
         session_id so you can keep interacting."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "number",
                    "description": "Identifier of the running exec_command session."
                },
                "chars": {
                    "type": "string",
                    "description": "Bytes to write to stdin. Empty (default) polls for output without writing."
                },
                "yield_time_ms": {
                    "type": "number",
                    "description": "Milliseconds to wait for output. With a write: default 250 (max 30000). Empty poll: default 5000 (max 300000)."
                }
            },
            "required": ["session_id"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        let id = input
            .get("session_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let chars = input.get("chars").and_then(|v| v.as_str()).unwrap_or("");
        if chars.is_empty() {
            format!("write_stdin: poll session_id={id}")
        } else {
            format!(
                "write_stdin: session_id={id} <- {}",
                crate::truncate_utf8(chars, 40)
            )
        }
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let id = match input.get("session_id").and_then(|v| v.as_u64()) {
            Some(i) => i,
            None => {
                return ToolResult::error("write_stdin: missing required parameter `session_id`");
            }
        };
        let chars = input.get("chars").and_then(|v| v.as_str()).unwrap_or("");

        let (pty, read_offset) = match self.store.touch_with_offset(id).await {
            Some(v) => v,
            None => {
                return ToolResult::error(format!(
                    "write_stdin: unknown or finished session_id={id}"
                ));
            }
        };

        if !chars.is_empty() {
            if let Err(e) = pty.write(chars.as_bytes()) {
                return ToolResult::error(format!("write_stdin: write failed: {e}"));
            }
            // Ctrl-C just needs the signal to land; other input gets a brief
            // reaction window before we start polling.
            if chars != INTERRUPT {
                tokio::time::sleep(Duration::from_millis(POST_WRITE_REACT_MS)).await;
            }
        }

        let yield_ms = {
            let t = input.get("yield_time_ms").and_then(|v| v.as_u64());
            if chars.is_empty() {
                t.unwrap_or(MIN_EMPTY_YIELD_MS)
                    .clamp(MIN_EMPTY_YIELD_MS, MAX_EMPTY_YIELD_MS)
            } else {
                t.unwrap_or(MIN_YIELD_MS).clamp(MIN_YIELD_MS, MAX_YIELD_MS)
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(yield_ms);
        let (collected, read_offset) = collect_until_deadline(&pty, read_offset, deadline).await;
        let text = truncate_middle(
            &String::from_utf8_lossy(&collected),
            TruncationBudget::Bytes(OUTPUT_CAP_BYTES),
        );

        if pty.has_exited() {
            let code = pty.exit_code().unwrap_or(-1);
            // The session is done — drop it from the store.
            self.store.remove(id).await;
            ToolResult::text(format!("(process exited, exit_code={code})\n{text}"))
        } else {
            self.store.update_read_offset(id, read_offset).await;
            ToolResult::text(format!("session_id={id}\n{text}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec_command::ExecCommandTool;
    use crate::test_support::pty_test_helper_shell_cmd;

    fn parse_session_id(content: &str) -> Option<u64> {
        content
            .lines()
            .find_map(|l| l.strip_prefix("session_id="))
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    #[tokio::test]
    async fn unknown_session_is_error() {
        let store = Arc::new(ProcessStore::new());
        let tool = WriteStdinTool::new(store);
        let r = tool.execute(serde_json::json!({"session_id": 4242})).await;
        assert!(r.is_error, "unknown session must error: {}", r.content);
    }

    #[tokio::test]
    async fn missing_session_id_is_error() {
        let store = Arc::new(ProcessStore::new());
        let tool = WriteStdinTool::new(store);
        let r = tool.execute(serde_json::json!({"chars": "x"})).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn cat_echoes_written_line() {
        let store = Arc::new(ProcessStore::new());
        let exec = ExecCommandTool::new(store.clone(), std::env::current_dir().unwrap());
        // The helper's `echo-stdin` echoes each written line (cross-platform `cat`).
        let r = exec
            .execute(serde_json::json!({
                "cmd": pty_test_helper_shell_cmd("echo-stdin"),
                "yield_time_ms": 400
            }))
            .await;
        let sid = parse_session_id(&r.content).expect("echo-stdin should return a session_id");

        let writer = WriteStdinTool::new(store.clone());
        let r2 = writer
            .execute(serde_json::json!({"session_id": sid, "chars": "hello_world\n", "yield_time_ms": 1500}))
            .await;
        assert!(!r2.is_error, "unexpected error: {}", r2.content);
        assert!(
            r2.content.contains("hello_world"),
            "echo-stdin should echo the written line, got: {}",
            r2.content
        );

        // Ctrl-C ends the helper; subsequent polling should observe the exit.
        let _ = writer
            .execute(serde_json::json!({"session_id": sid, "chars": "\u{3}", "yield_time_ms": 800}))
            .await;
        store.terminate_all().await;
    }

    // `sh -i` REPL semantics are unix-specific (there is no portable interactive
    // shell reachable through the `<shell> <flag>` wrapper). The cross-platform
    // write→echo round-trip through the full tool stack is covered by
    // `cat_echoes_written_line` above; this keeps the genuine REPL-eval check on
    // the platform that has one.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_repl_evaluates_expression() {
        let store = Arc::new(ProcessStore::new());
        let exec = ExecCommandTool::new(store.clone(), std::env::current_dir().unwrap());
        let r = exec
            .execute(serde_json::json!({"cmd": "sh -i", "yield_time_ms": 500}))
            .await;
        let sid = match parse_session_id(&r.content) {
            Some(s) => s,
            // Some CI shells exit `sh -i` without a controlling tty quirk; skip
            // rather than flake (environmental, per test-workflow-rules-macos).
            None => return,
        };
        let writer = WriteStdinTool::new(store.clone());
        // Send the expression and the newline together here — a plain REPL (not a
        // TUI) accepts the burst; the split-Enter guidance is for TUIs.
        let r2 = writer
            .execute(serde_json::json!({"session_id": sid, "chars": "echo $((6*7))\n", "yield_time_ms": 1500}))
            .await;
        assert!(
            r2.content.contains("42"),
            "sh REPL should evaluate 6*7=42, got: {}",
            r2.content
        );
        store.terminate_all().await;
    }

    #[tokio::test]
    async fn empty_poll_picks_up_delayed_output() {
        let store = Arc::new(ProcessStore::new());
        let exec = ExecCommandTool::new(store.clone(), std::env::current_dir().unwrap());
        // Emits "late_line" after ~300ms, then keeps the session alive ~5s. The
        // helper's `emit-after` is deterministic and cross-platform (replaces the
        // shell-specific `sleep 0.3; echo ...; sleep 5`).
        let r = exec
            .execute(serde_json::json!({
                "cmd": pty_test_helper_shell_cmd("emit-after 300 late_line 5000"),
                "yield_time_ms": 100
            }))
            .await;
        let sid = parse_session_id(&r.content)
            .expect("process should still be running at 100ms (it sleeps first)");
        assert!(
            !r.content.contains("late_line"),
            "late_line should NOT appear in the first 100ms window: {}",
            r.content
        );

        let writer = WriteStdinTool::new(store.clone());
        // Empty poll (no write) with a generous window must catch the late line.
        let r2 = writer
            .execute(serde_json::json!({"session_id": sid, "chars": "", "yield_time_ms": 5000}))
            .await;
        assert!(
            r2.content.contains("late_line"),
            "empty poll should pick up delayed output, got: {}",
            r2.content
        );
        store.terminate_all().await;
    }

    #[tokio::test]
    async fn empty_poll_picks_up_output_emitted_between_tool_calls() {
        let store = Arc::new(ProcessStore::new());
        let exec = ExecCommandTool::new(store.clone(), std::env::current_dir().unwrap());
        let r = exec
            .execute(serde_json::json!({
                "cmd": pty_test_helper_shell_cmd("emit-after 300 gap_line 6000"),
                "yield_time_ms": 100
            }))
            .await;
        let sid = parse_session_id(&r.content)
            .expect("process should still be running at 100ms (it emits later)");

        tokio::time::sleep(Duration::from_millis(700)).await;

        let writer = WriteStdinTool::new(store.clone());
        let r2 = writer
            .execute(serde_json::json!({"session_id": sid, "chars": "", "yield_time_ms": 5000}))
            .await;
        assert!(
            r2.content.contains("gap_line"),
            "empty poll should replay output produced while no receiver was subscribed, got: {}",
            r2.content
        );
        store.terminate_all().await;
    }
}
