use std::sync::Arc;

use nomifun_common::{AppError, CommandSpec, ErrorChain};
use nomi_process_runtime::ChildProcessBuilder as CmdBuilder;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, error, info, trace, warn};

use super::{
    CliAgentProcess, EVENT_CHANNEL_CAPACITY, STDERR_BUFFER_MAX, prepare_command_cwd,
    spawn_exit_monitor, tracked_process_group_id,
};

impl CliAgentProcess {
    /// Spawn a new CLI subprocess in **JSON-lines mode**.
    ///
    /// The child process is started with stdin, stdout, and stderr piped.
    /// Background tasks are spawned to:
    /// - Read stdout line-by-line and parse each line as JSON
    /// - Read stderr and buffer the last [`STDERR_BUFFER_MAX`] bytes
    /// - Monitor process exit
    ///
    /// This is used by Gemini, OpenClaw, Nanobot agents.
    pub async fn spawn(config: CommandSpec) -> Result<Self, AppError> {
        let mut cmd = CmdBuilder::new(&config.command);
        cmd.args(&config.args)
            .envs(config.env.iter().map(|e| (&e.name, &e.value)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(prepare_command_cwd(cwd)?);
        }

        let preview = cmd.to_string();
        info!(command = %preview, "Spawning CLI process");
        let mut child: Child = cmd.spawn().map_err(|e| {
            error!(command = %preview, error = %ErrorChain(&e), "Failed to spawn CLI process");
            AppError::Internal(format!("Failed to spawn CLI process '{preview}': {e}"))
        })?;

        let pid = child
            .id()
            .ok_or_else(|| AppError::Internal("Failed to obtain PID from spawned process".into()))?;
        info!(pid, command = %preview, "CLI process spawned");

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Internal("Failed to capture stdout from child process".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Internal("Failed to capture stderr from child process".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Internal("Failed to capture stdin for child process".into()))?;

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        // Pre-subscribe before spawning background tasks to guarantee no events are lost
        let initial_rx = event_tx.subscribe();

        // Background task: read stdout line-by-line → parse JSON → broadcast
        let stdout_tx = event_tx.clone();
        let stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(value) => {
                        // Ignore send errors — no active subscribers is fine
                        let _ = stdout_tx.send(value);
                    }
                    Err(e) => {
                        trace!(line = trimmed, error = %e, "Non-JSON line from stdout, skipping");
                    }
                }
            }

            debug!(pid, "Stdout reader finished");
        });

        // Background task: read stderr → ring buffer + log
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buffer);
        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(pid, stderr = trimmed, "CLI process stderr");
                }
                let mut buf = stderr_buf_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > STDERR_BUFFER_MAX {
                    let cut = buf.len() - STDERR_BUFFER_MAX;
                    buf.drain(..cut);
                }
            }

            debug!(pid, "Stderr reader finished");
        });

        let (force_kill_tx, exit_rx, exit_handle) = spawn_exit_monitor(child, pid);

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(None), // stdout consumed by reader task
            pid,
            process_group_id: tracked_process_group_id(pid),
            event_tx,
            force_kill_tx,
            exit_rx,
            initial_rx: std::sync::Mutex::new(Some(initial_rx)),
            stderr_buffer,
            _stdout_handle: Some(Arc::new(stdout_handle)),
            _stderr_handle: Arc::new(stderr_handle),
            _exit_handle: Arc::new(exit_handle),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{echo_json_config, simple_script_config};
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::timeout;

    // ── JSON-lines mode tests ────────────────────────────────────────────

    #[tokio::test]
    async fn spawn_and_receive_event() {
        let config = echo_json_config(r#"{"type":"text","data":{"content":"hello"}}"#);
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();

        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out waiting for event")
            .expect("Channel closed");

        assert_eq!(event["type"], "text");
        assert_eq!(event["data"]["content"], "hello");

        let status = timeout(Duration::from_secs(5), proc.wait_for_exit())
            .await
            .expect("Timed out waiting for exit");
        assert!(status.is_some());
    }

    #[tokio::test]
    async fn spawn_multiple_events() {
        let script = r#"echo '{"type":"start","data":{}}' && echo '{"type":"text","data":{"content":"line1"}}' && echo '{"type":"finish","data":{}}'  "#;
        let config = simple_script_config(script);
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();

        let mut events = Vec::new();
        for _ in 0..3 {
            let event = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("Timed out")
                .expect("Channel closed");
            events.push(event);
        }

        assert_eq!(events[0]["type"], "start");
        assert_eq!(events[1]["type"], "text");
        assert_eq!(events[2]["type"], "finish");
    }

    #[tokio::test]
    async fn non_json_lines_are_skipped() {
        let script = r#"echo 'not json' && echo '{"type":"ok","data":{}}' && echo 'also not json'"#;
        let config = simple_script_config(script);
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();

        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");

        assert_eq!(event["type"], "ok");
        proc.wait_for_exit().await;
    }

    #[tokio::test]
    async fn empty_lines_are_skipped() {
        let script = "echo '' && echo '  ' && echo '{\"type\":\"data\",\"data\":{}}' && echo ''";
        let config = simple_script_config(script);
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();

        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");
        assert_eq!(event["type"], "data");
    }

    #[tokio::test]
    async fn send_json_to_stdin() {
        let config = simple_script_config("read line && echo \"$line\"");
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();

        let msg = json!({"type": "sendMessage", "data": {"content": "test"}});
        proc.send(&msg).await.unwrap();
        proc.close_stdin().await;

        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");
        assert_eq!(event["type"], "sendMessage");
        assert_eq!(event["data"]["content"], "test");
    }

    #[tokio::test]
    async fn send_after_exit_returns_error() {
        let config = simple_script_config("true");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        proc.wait_for_exit().await;
        proc.close_stdin().await;

        let result = proc.send(&json!({"type":"test"})).await;
        assert!(result.is_err());
    }
}
