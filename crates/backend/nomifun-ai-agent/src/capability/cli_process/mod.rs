use std::fs;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{AppError, workspace_path_has_edge_whitespace_segment};
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, broadcast, oneshot, watch};
use tracing::{debug, error, warn};

mod spawn_json_lines;
mod spawn_sdk;
mod stderr_monitor;
mod process_monitor;

use process_monitor::{
    ForceKillRequest, ForceKillSender, ProcessExitState, spawn_exit_monitor,
    wait_for_terminal_state,
};

/// Wrapper to hold a pre-subscribed receiver from before background tasks start.
/// Ensures no events are lost between process spawn and consumer subscription.
type InitialReceiver = std::sync::Mutex<Option<broadcast::Receiver<serde_json::Value>>>;

/// Default broadcast channel capacity for stdout events.
pub(super) const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Maximum stderr ring-buffer size in bytes.
pub(super) const STDERR_BUFFER_MAX: usize = 8192;

pub(super) fn prepare_command_cwd(cwd: &str) -> Result<PathBuf, AppError> {
    if cwd.trim().is_empty() {
        return Err(AppError::BadRequest("Workspace directory is empty".into()));
    }

    let workspace_path = PathBuf::from(cwd);
    if workspace_path_has_edge_whitespace_segment(&workspace_path) {
        return Err(AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(
            workspace_path.display().to_string(),
        ));
    }

    match fs::metadata(&workspace_path) {
        Ok(metadata) if metadata.is_dir() => Ok(workspace_path),
        Ok(_) => Err(AppError::BadRequest(format!(
            "Workspace path is not a directory: {}",
            workspace_path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AppError::BadRequest(format!(
            "Workspace directory does not exist: {}",
            workspace_path.display()
        ))),
        Err(e) => Err(AppError::BadRequest(format!(
            "Workspace directory is not accessible: {}: {}",
            workspace_path.display(),
            e
        ))),
    }
}

/// Manages a CLI subprocess with optional JSON-over-stdin/stdout communication.
///
/// Supports two modes:
///
/// 1. **JSON-lines mode** (Gemini, OpenClaw, Nanobot): stdout is read as line-delimited
///    JSON and broadcast via `subscribe()`. Messages are sent via `send()`.
///
/// 2. **SDK mode** (ACP): call [`take_stdio`](Self::take_stdio) to hand raw
///    stdin/stdout to the ACP SDK transport. After this, `send()` and `subscribe()`
///    are no longer available.
pub struct CliAgentProcess {
    /// Stdin writer, wrapped in Mutex for concurrent send safety.
    /// Set to `None` once stdin is closed, taken, or process exited.
    stdin: Mutex<Option<ChildStdin>>,
    /// Raw stdout handle. Only available before background tasks start or
    /// in SDK mode (taken by `take_stdio`). `None` once consumed.
    stdout: Mutex<Option<ChildStdout>>,
    /// OS-level process ID.
    pid: u32,
    /// Process group ID captured at spawn time so teardown can still target
    /// the whole tree after the direct child exits.
    process_group_id: Option<u32>,
    /// Broadcast sender for parsed stdout events (JSON-lines mode only).
    #[allow(dead_code)] // Part of the complete CliProcess API; used in JSON-lines mode via subscribe()
    event_tx: broadcast::Sender<serde_json::Value>,
    /// Exact-child control channel owned by the exit monitor.
    force_kill_tx: ForceKillSender,
    /// Watch channel that always reaches an explicit terminal state, including
    /// monitor/cleanup failure; failure must never masquerade as Running.
    exit_rx: watch::Receiver<ProcessExitState>,
    /// Pre-subscribed receiver created before background tasks start (JSON-lines mode).
    /// Take this via [`take_initial_receiver`] to guarantee no events are lost.
    initial_rx: InitialReceiver,
    /// Stderr ring buffer for diagnostics.
    #[allow(dead_code)] // Read via take_stderr(); part of diagnostics API for startup crash reporting
    stderr_buffer: Arc<Mutex<String>>,
    /// Handle to the stdout reader task (JSON-lines mode, for cleanup).
    _stdout_handle: Option<Arc<tokio::task::JoinHandle<()>>>,
    /// Handle to the stderr reader task (for cleanup).
    _stderr_handle: Arc<tokio::task::JoinHandle<()>>,
    /// Handle to the exit monitor task (for cleanup).
    _exit_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl CliAgentProcess {
    /// Take ownership of stdin and stdout for the SDK transport.
    ///
    /// Only available in SDK mode (after [`spawn_for_sdk`](Self::spawn_for_sdk)).
    /// Can only be called once. Returns `None` on subsequent calls or if
    /// spawned in JSON-lines mode.
    pub async fn take_stdio(&self) -> Option<(ChildStdin, ChildStdout)> {
        let stdin = self.stdin.lock().await.take()?;
        let stdout = self.stdout.lock().await.take()?;
        Some((stdin, stdout))
    }

    /// Send a JSON message to the subprocess via stdin (JSON-lines mode).
    ///
    /// The message is serialized as a single line followed by a newline.
    /// Returns an error if stdin has been closed (process exited) or taken
    /// by [`take_stdio`](Self::take_stdio).
    pub async fn send(&self, message: &serde_json::Value) -> Result<(), AppError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| AppError::Internal("Cannot send: stdin is closed (process exited or taken)".into()))?;

        let mut buf =
            serde_json::to_vec(message).map_err(|e| AppError::Internal(format!("Failed to serialize message: {e}")))?;
        buf.push(b'\n');

        stdin.write_all(&buf).await.map_err(|e| {
            error!(pid = self.pid, error = %e, "Failed to write to stdin");
            AppError::Internal(format!("Failed to write to stdin: {e}"))
        })?;

        stdin.flush().await.map_err(|e| {
            error!(pid = self.pid, error = %e, "Failed to flush stdin");
            AppError::Internal(format!("Failed to flush stdin: {e}"))
        })?;

        Ok(())
    }

    /// Subscribe to the event stream from stdout (JSON-lines mode).
    ///
    /// Returns a broadcast receiver that yields raw `serde_json::Value` events
    /// as they are parsed from the subprocess stdout.
    #[allow(dead_code)] // Complete CliProcess API for JSON-lines-mode event subscription
    pub fn subscribe(&self) -> broadcast::Receiver<serde_json::Value> {
        self.event_tx.subscribe()
    }

    /// Take the pre-subscribed receiver created before background tasks started
    /// (JSON-lines mode).
    ///
    /// This receiver captures all events from the very first output line.
    /// Can only be called once; subsequent calls return `None`.
    pub fn take_initial_receiver(&self) -> Option<broadcast::Receiver<serde_json::Value>> {
        self.initial_rx.lock().unwrap().take()
    }

    /// Close stdin, signaling the subprocess that no more input will arrive.
    pub async fn close_stdin(&self) {
        let mut guard = self.stdin.lock().await;
        if guard.take().is_some() {
            debug!(pid = self.pid, "Stdin closed");
        }
    }

    /// Gracefully terminate the subprocess.
    ///
    /// 1. Close stdin
    /// 2. Wait up to `grace_period` for the process to exit on its own
    /// 3. If still running after grace period, send SIGKILL
    pub async fn kill(&self, grace_period: Duration) -> Result<(), AppError> {
        // Close stdin first to signal the child
        self.close_stdin().await;

        // Wait for graceful exit within the grace period
        let mut rx = self.exit_rx.clone();
        if let Ok(state) = tokio::time::timeout(grace_period, wait_for_terminal_state(&mut rx)).await {
            if let Some(error) = state.failure() {
                return Err(AppError::Internal(error.to_owned()));
            }
            debug!(pid = self.pid, "CLI process exited gracefully");
            return Ok(());
        }

        // Force-kill through the task that owns the exact Child handle. The
        // shared process runtime terminates its registered group/Job Object and
        // proves tree cleanup, without a PID-reuse race or `taskkill` process.
        warn!(pid = self.pid, "Grace period expired, sending SIGKILL");
        let (completion, completed) = oneshot::channel();
        if self
            .force_kill_tx
            .send(ForceKillRequest {
                completion: Some(completion),
            })
            .is_err()
        {
            let state = self.exit_rx.borrow().clone();
            return match state.failure() {
                Some(error) => Err(AppError::Internal(error.to_owned())),
                None if !state.is_running() => Ok(()),
                None => Err(AppError::Internal(format!(
                    "Process {} force-kill monitor is unavailable",
                    self.pid
                ))),
            };
        }

        match tokio::time::timeout(Duration::from_secs(7), completed).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(AppError::Internal(error)),
            Ok(Err(_)) => {
                // Another concurrent force request may have won and completed
                // the single monitor task. Its authoritative terminal state is
                // the result for every waiter.
                let mut rx = self.exit_rx.clone();
                match tokio::time::timeout(Duration::from_secs(1), wait_for_terminal_state(&mut rx)).await {
                    Ok(state) => match state.failure() {
                        Some(error) => Err(AppError::Internal(error.to_owned())),
                        None => Ok(()),
                    },
                    Err(_) => Err(AppError::Internal(format!(
                        "Process {} force-kill request closed without a terminal state",
                        self.pid
                    ))),
                }
            }
            Err(_) => Err(AppError::Internal(format!(
                "Process {} did not terminate within the exact tree-cleanup deadline",
                self.pid
            ))),
        }
    }

    /// Check whether the subprocess is still running.
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub fn is_running(&self) -> bool {
        self.exit_rx.borrow().is_running()
    }

    /// Get the exit status if the process has exited.
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_rx.borrow().exit_status()
    }

    /// Get the OS process ID.
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Get the cached process group ID captured when the child was spawned.
    pub fn process_group_id(&self) -> Option<u32> {
        self.process_group_id
    }

    /// Wait for the process to exit (blocks until exit or cancellation).
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub async fn wait_for_exit(&self) -> Option<ExitStatus> {
        let mut rx = self.exit_rx.clone();
        wait_for_terminal_state(&mut rx).await.exit_status()
    }

    /// Clone the exit observation channel without retaining the process
    /// wrapper itself. Lifecycle bookkeeping tasks must use this instead of
    /// holding an `Arc<CliAgentProcess>` until exit; otherwise the bookkeeping
    /// task prevents the wrapper's last-owner `Drop` backstop from ever
    /// force-killing a process abandoned by a cancelled factory future.
    pub(crate) fn exit_receiver(&self) -> watch::Receiver<ProcessExitState> {
        self.exit_rx.clone()
    }

    /// Take the buffered stderr content (consuming).
    ///
    /// Returns the last [`STDERR_BUFFER_MAX`] bytes of stderr output.
    /// Used for error diagnostics in `AcpError::StartupCrash` and
    /// `AcpError::Disconnected`.
    #[allow(dead_code)] // Diagnostics API for startup crash and disconnect error reporting
    pub async fn take_stderr(&self) -> String {
        let mut buf = self.stderr_buffer.lock().await;
        std::mem::take(&mut *buf)
    }

    /// Peek the last `max_lines` newline-delimited lines from the stderr ring
    /// buffer **without draining**.
    ///
    /// Used by error-augmentation paths (`AcpAgentManager::send_message`) that
    /// need to surface tracing-level error context the SDK didn't include in
    /// its JSON-RPC response. Returns an owned `String`; the buffer lock is
    /// held for the duration of this call (microseconds at the bounded sizes
    /// we read) and dropped before the result is returned.
    ///
    /// `max_lines == 0` returns an empty string. The returned string has no
    /// trailing newline — the caller may append one if they want.
    #[allow(dead_code)] // Called by error-augmentation path in AcpAgentManager::send_message (Task 5)
    pub async fn peek_stderr_tail(&self, max_lines: usize) -> String {
        if max_lines == 0 {
            return String::new();
        }
        let buf = self.stderr_buffer.lock().await;
        let trimmed = buf.trim_end_matches('\n');
        if trimmed.is_empty() {
            return String::new();
        }
        // `rsplit('\n')` walks lines from the end. Take up to `max_lines`,
        // then re-collect into the original top-to-bottom order.
        let mut tail: Vec<&str> = trimmed.rsplit('\n').take(max_lines).collect();
        tail.reverse();
        tail.join("\n")
    }
}

impl Drop for CliAgentProcess {
    fn drop(&mut self) {
        // Runtime factories can be cancelled while a CLI is between spawn and
        // protocol handshake. At that point no manager exists yet to call
        // `kill()`. Make last-owner drop a hard lifecycle backstop so dropping
        // a cancelled factory future cannot orphan the child/process tree.
        if self.exit_rx.borrow().is_running() {
            warn!(pid = self.pid, "CLI process owner dropped while child was still running; scheduling exact tree cleanup");
            if self
                .force_kill_tx
                .send(ForceKillRequest { completion: None })
                .is_err()
            {
                // A missing monitor means its Child future has already been
                // dropped; ChildProcessBuilder's kill-on-drop and Job/group
                // reaper are then the ownership backstop.
                error!(pid = self.pid, "Exact-child cleanup monitor was unavailable during Drop");
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn tracked_process_group_id(pid: u32) -> Option<u32> {
    Some(pid)
}

#[cfg(not(unix))]
pub(super) fn tracked_process_group_id(_pid: u32) -> Option<u32> {
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use nomifun_common::{CommandSpec, EnvVar};

    use super::*;
    use tokio::time::timeout;

    #[cfg(windows)]
    pub(crate) fn test_shell_program() -> std::path::PathBuf {
        ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(std::path::PathBuf::from)
            .map(|root| root.join("Git").join("bin").join("sh.exe"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| "sh".into())
    }

    #[cfg(not(windows))]
    pub(crate) fn test_shell_program() -> std::path::PathBuf {
        "sh".into()
    }

    pub(crate) fn echo_json_config(json_str: &str) -> CommandSpec {
        simple_script_config(&format!("echo '{json_str}'"))
    }

    pub(crate) fn simple_script_config(script: &str) -> CommandSpec {
        CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            cwd: None,
        }
    }

    // ── Lifecycle tests (apply to both modes) ────────────────────────

    #[tokio::test]
    async fn is_running_reflects_process_state() {
        let config = simple_script_config("sleep 10");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        assert!(proc.is_running());
        assert!(proc.exit_status().is_none());

        proc.kill(Duration::from_millis(100)).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        assert!(!proc.is_running());
        assert!(proc.exit_status().is_some());
    }

    #[tokio::test]
    async fn kill_with_grace_period_exits_cleanly() {
        let config = simple_script_config("read line");
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        assert!(proc.is_running());

        proc.kill(Duration::from_secs(5)).await.unwrap();
        assert!(!proc.is_running());
    }

    #[tokio::test]
    async fn kill_force_kills_after_grace_period() {
        let config = simple_script_config("trap '' TERM; while true; do sleep 1; done");
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        assert!(proc.is_running());

        let result = proc.kill(Duration::from_millis(100)).await;
        assert!(result.is_ok());

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        assert!(!proc.is_running());
    }

    #[tokio::test]
    async fn spawn_with_env_and_cwd() {
        let config = CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), "echo \"{\\\"val\\\":\\\"$MY_TEST_VAR\\\"}\"".into()],
            env: vec![EnvVar {
                name: "MY_TEST_VAR".into(),
                value: "hello_env".into(),
            }],
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        };
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();

        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");
        assert_eq!(event["val"], "hello_env");
    }

    #[tokio::test]
    async fn spawn_rejects_cwd_with_trailing_whitespace_in_request() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        fs::create_dir(&cwd).unwrap();
        let cwd_with_trailing_space = format!("{} ", cwd.to_string_lossy());

        let config = CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), "echo \"{\\\"cwd\\\":\\\"$PWD\\\"}\"".into()],
            env: vec![],
            cwd: Some(cwd_with_trailing_space.clone()),
        };
        let result = CliAgentProcess::spawn(config).await;
        assert!(matches!(
            result,
            Err(AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(message))
                if message == cwd_with_trailing_space
        ));
    }

    #[tokio::test]
    async fn spawn_accepts_cwd_with_interior_whitespace_segment() {
        let dir = tempfile::tempdir().unwrap();
        // Mirrors the macOS data dir layout ("Application Support").
        let workspace_parent = dir.path().join("my workspace");
        fs::create_dir(&workspace_parent).unwrap();
        let cwd = workspace_parent.join("project");
        fs::create_dir(&cwd).unwrap();

        let config = CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), "echo \"{\\\"cwd\\\":\\\"$PWD\\\"}\"".into()],
            env: vec![],
            cwd: Some(cwd.to_string_lossy().into_owned()),
        };

        let proc = CliAgentProcess::spawn(config).await.unwrap();
        let mut rx = proc.subscribe();
        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");
        let reported = event["cwd"].as_str().unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::canonicalize(reported).unwrap(),
            fs::canonicalize(&cwd).unwrap(),
            "process must actually run inside the interior-whitespace cwd"
        );
        #[cfg(windows)]
        // Git Bash 的 `$PWD` 是 POSIX 风格（如 /c/Users/...），无法 canonicalize；
        // 断言带内部空格的两级目录段原样存活即可证明进程确实运行在该 cwd 内。
        assert!(
            reported.ends_with("my workspace/project"),
            "process must actually run inside the interior-whitespace cwd, got: {reported}"
        );
    }

    #[tokio::test]
    async fn spawn_for_sdk_accepts_cwd_with_interior_whitespace_segment() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_parent = dir.path().join("my workspace");
        fs::create_dir(&workspace_parent).unwrap();
        let cwd = workspace_parent.join("project");
        fs::create_dir(&cwd).unwrap();
        let data_dir = tempfile::tempdir().unwrap();

        let config = CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), "echo ready".into()],
            env: vec![],
            cwd: Some(cwd.to_string_lossy().into_owned()),
        };

        let proc = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await.unwrap();
        proc.kill(Duration::from_millis(100)).await.unwrap();
    }

    #[tokio::test]
    async fn spawn_rejects_missing_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let missing_cwd = dir.path().join("missing").join("workspace");
        assert!(!missing_cwd.exists());

        let config = CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), "echo \"{\\\"cwd\\\":\\\"$PWD\\\"}\"".into()],
            env: vec![],
            cwd: Some(missing_cwd.to_string_lossy().into_owned()),
        };

        let result = CliAgentProcess::spawn(config).await;
        assert!(matches!(
            result,
            Err(AppError::BadRequest(message)) if message.contains("Workspace directory does not exist")
        ));
        assert!(!missing_cwd.exists());
    }

    #[tokio::test]
    async fn spawn_for_sdk_rejects_missing_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let missing_cwd = dir.path().join("missing-sdk").join("workspace");
        assert!(!missing_cwd.exists());

        let config = CommandSpec {
            command: test_shell_program(),
            args: vec!["-c".into(), "sleep 10".into()],
            env: vec![],
            cwd: Some(missing_cwd.to_string_lossy().into_owned()),
        };

        let result = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await;
        assert!(matches!(
            result,
            Err(AppError::BadRequest(message)) if message.contains("Workspace directory does not exist")
        ));
        assert!(!missing_cwd.exists());
    }

    #[tokio::test]
    async fn spawn_invalid_command_returns_error() {
        let config = CommandSpec {
            command: "/nonexistent/binary/that/does/not/exist".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let result = CliAgentProcess::spawn(config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pid_is_nonzero_for_valid_process() {
        let config = simple_script_config("sleep 10");
        let proc = CliAgentProcess::spawn(config).await.unwrap();
        assert!(proc.pid() > 0);
        proc.kill(Duration::from_millis(100)).await.unwrap();
    }

    #[tokio::test]
    async fn wait_for_exit_returns_immediately_if_already_exited() {
        let config = simple_script_config("true");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        let status1 = timeout(Duration::from_secs(5), proc.wait_for_exit())
            .await
            .expect("Timed out");
        assert!(status1.is_some());

        let status2 = timeout(Duration::from_millis(100), proc.wait_for_exit())
            .await
            .expect("Should return immediately");
        assert!(status2.is_some());
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_events() {
        let config = echo_json_config(r#"{"type":"broadcast","data":{"msg":"all"}}"#);
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        let mut rx1 = proc.subscribe();
        let mut rx2 = proc.subscribe();

        let e1 = timeout(Duration::from_secs(5), rx1.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");
        let e2 = timeout(Duration::from_secs(5), rx2.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");

        assert_eq!(e1, e2);
        assert_eq!(e1["type"], "broadcast");
    }
}
