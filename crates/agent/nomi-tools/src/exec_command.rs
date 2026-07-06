//! `exec_command`: start a long-lived command in a PTY and return either its
//! output (if it exits within `yield_time_ms`) or a `session_id` the model can
//! drive with `write_stdin`. Lets the model run REPLs (python/node), TUIs, and
//! interactive installers.
//!
//! Shares an `Arc<ProcessStore>` with `WriteStdinTool` (constructed once in
//! bootstrap, cloned into both) — the same stateful-tool pattern as `SpawnTool`
//! / `BrowserTool`. No `Tool` trait change.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_config::shell::{shell_command_args, shell_info};
use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};

use crate::Tool;
use crate::output_truncation::{TruncationBudget, truncate_middle};
use crate::process_store::{ExecSession, ProcessStore, collect_until_deadline};
use crate::pty::{Pty, PtyParams};

const DEFAULT_YIELD_MS: u64 = 10_000;
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
/// Output byte budget per call, head/tail elided via the shared truncator.
const OUTPUT_CAP_BYTES: usize = 128 * 1024;

pub struct ExecCommandTool {
    store: Arc<ProcessStore>,
    default_cwd: PathBuf,
    /// Shell program used to run command strings.
    shell_program: String,
}

impl ExecCommandTool {
    pub fn new(store: Arc<ProcessStore>, cwd: PathBuf) -> Self {
        let info = shell_info();
        Self {
            store,
            default_cwd: cwd,
            shell_program: info.program.to_string(),
        }
    }
}

#[async_trait]
impl Tool for ExecCommandTool {
    fn name(&self) -> &str {
        "exec_command"
    }

    fn description(&self) -> &str {
        "Runs a command in a PTY, returning its output or a session_id for ongoing interaction.\n\n\
         The command is executed by the platform shell. On Windows this is PowerShell \
         (use PowerShell syntax such as Get-ChildItem, $env:NAME, and ';' for sequencing; \
         run cmd /C \"...\" explicitly when cmd.exe syntax is required). On macOS/Linux this \
         is POSIX sh.\n\n\
         Use this for long-lived, interactive processes: REPLs (python, node), TUIs, and \
         interactive installers — things the one-shot Bash tool cannot drive.\n\n\
         - If the process exits within yield_time_ms, the result reports its exit_code and NO \
         session_id.\n\
         - If it is still running, the result includes a session_id — feed further input with \
         write_stdin (chars=\"\" polls for more output without writing).\n\n\
         IMPORTANT (TUI submit): when driving an interactive program, send the Enter/return key \
         (\"\\r\") as its OWN separate write_stdin call, after writing the line of text. Sending \
         text and the carriage return in a single burst can be swallowed by a TUI's paste-burst \
         detection, leaving the command unsubmitted."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory. Defaults to the session cwd."
                },
                "tty": {
                    "type": "boolean",
                    "description": "Allocate a fuller PTY window for TUI programs. Defaults to false."
                },
                "yield_time_ms": {
                    "type": "number",
                    "description": "Milliseconds to wait for output before yielding. Default 10000, range 250-30000."
                }
            },
            "required": ["cmd"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false // PTY sessions are serialized.
    }

    fn category(&self) -> ToolCategory {
        // Same trust level as Bash: it can run arbitrary commands, so it goes
        // through the same approval gating, not Info.
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        let c = input.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        format!("exec_command: {}", crate::truncate_utf8(c, 80))
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let cmd = match input.get("cmd").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return ToolResult::error("exec_command: missing required parameter `cmd`"),
        };
        let cwd = input
            .get("workdir")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| self.default_cwd.to_string_lossy().into_owned());
        let tty = input.get("tty").and_then(|v| v.as_bool()).unwrap_or(false);
        let yield_ms = input
            .get("yield_time_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_YIELD_MS)
            .clamp(MIN_YIELD_MS, MAX_YIELD_MS);

        // Run through the platform shell, mirroring nomi's Bash tool.
        let params = PtyParams {
            program: self.shell_program.clone(),
            args: shell_command_args(&cmd),
            cwd,
            env: std::env::vars().collect(),
            cols: if tty { 120 } else { 80 },
            rows: if tty { 30 } else { 24 },
        };
        let pty = match Pty::spawn(params) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("exec_command: spawn failed: {e}")),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(yield_ms);
        let (collected, read_offset) = collect_until_deadline(&pty, 0, deadline).await;
        let text = truncate_middle(
            &String::from_utf8_lossy(&collected),
            TruncationBudget::Bytes(OUTPUT_CAP_BYTES),
        );

        if pty.has_exited() {
            let code = pty.exit_code().unwrap_or(-1);
            ToolResult::text(format!("(process exited, exit_code={code})\n{text}"))
        } else {
            let (id, pruned) = self
                .store
                .insert(ExecSession {
                    id: 0,
                    pty: pty.clone(),
                    read_offset,
                    command: cmd,
                    tty,
                    last_used: tokio::time::Instant::now(),
                })
                .await;
            // Kill the evicted session's process OUTSIDE the store lock.
            if let Some(victim) = pruned {
                victim.kill();
            }
            ToolResult::text(format!(
                "session_id={id}\n(process still running — use write_stdin to continue)\n{text}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_session_id(content: &str) -> Option<u64> {
        content
            .lines()
            .find_map(|l| l.strip_prefix("session_id="))
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    #[tokio::test]
    async fn immediate_exit_reports_exit_code_no_session() {
        let store = Arc::new(ProcessStore::new());
        let tool = ExecCommandTool::new(store, std::env::current_dir().unwrap());
        let r = tool
            .execute(serde_json::json!({"cmd": "echo done_marker", "yield_time_ms": 3000}))
            .await;
        assert!(!r.is_error, "unexpected error: {}", r.content);
        assert!(r.content.contains("exit_code=0"), "got: {}", r.content);
        assert!(r.content.contains("done_marker"), "got: {}", r.content);
        assert!(
            parse_session_id(&r.content).is_none(),
            "should not get a session_id: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn long_lived_returns_session_id() {
        use crate::test_support::pty_test_helper_shell_cmd;
        let store = Arc::new(ProcessStore::new());
        let tool = ExecCommandTool::new(store.clone(), std::env::current_dir().unwrap());
        // The helper's `echo-stdin` blocks on stdin → stays alive past the short
        // yield (cross-platform stand-in for `cat`).
        let r = tool
            .execute(serde_json::json!({
                "cmd": pty_test_helper_shell_cmd("echo-stdin"),
                "yield_time_ms": 400
            }))
            .await;
        assert!(!r.is_error, "unexpected error: {}", r.content);
        let sid = parse_session_id(&r.content)
            .expect("echo-stdin should stay alive and return a session_id");
        assert!(store.contains(sid).await, "session should be in the store");
        // Clean up.
        store.terminate_all().await;
    }

    #[tokio::test]
    async fn missing_cmd_is_error() {
        let store = Arc::new(ProcessStore::new());
        let tool = ExecCommandTool::new(store, std::env::current_dir().unwrap());
        let r = tool.execute(serde_json::json!({})).await;
        assert!(r.is_error);
    }
}
