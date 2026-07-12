use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use nomi_execution::{
    CapabilityPolicy, CleanupReport, CommandSpec, ExecutionError, ExecutionOutcome,
    ExecutionOwner, ExecutionPolicy, OutputCursor, OutputSnapshot, OutputStream, PollResult,
    ProcessSupervisor, ShellKind, Transport, normalize_request,
};
use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::Tool;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const BASH_OUTPUT_MAX_BYTES: usize = 48_000;
const COMMAND_TIMEOUT_GUIDANCE: &str = "\
The command was stopped before completion. Do not assume its side effects finished. \
Inspect the partial state before retrying or running dependent steps.";

pub struct BashTool {
    supervisor: Arc<ProcessSupervisor>,
    cwd: PathBuf,
    capability: CapabilityPolicy,
    run_id: Uuid,
}

impl BashTool {
    pub fn new(
        supervisor: Arc<ProcessSupervisor>,
        cwd: PathBuf,
        capability: CapabilityPolicy,
    ) -> Self {
        Self {
            supervisor,
            cwd,
            capability,
            run_id: Uuid::now_v7(),
        }
    }

    async fn run_supervised(
        supervisor: Arc<ProcessSupervisor>,
        cwd: PathBuf,
        capability: CapabilityPolicy,
        owner: ExecutionOwner,
        command: String,
        timeout_ms: u64,
        cancelled: CancellationToken,
    ) -> ToolResult {
        let started_at = Instant::now();
        let deadline = started_at
            .checked_add(Duration::from_millis(timeout_ms))
            .unwrap_or(started_at);
        let request = nomi_execution::ExecutionRequest {
            owner,
            command: CommandSpec::Shell {
                shell: if cfg!(windows) {
                    ShellKind::PowerShell
                } else {
                    ShellKind::Posix
                },
                script: command,
            },
            cwd: cwd.clone(),
            env: BTreeMap::new(),
            transport: Transport::Pipe,
            policy: ExecutionPolicy {
                output_limit_bytes: BASH_OUTPUT_MAX_BYTES,
                deadline: Some(deadline),
                ..ExecutionPolicy::default()
            },
            capability,
        };
        let request = match normalize_request(request, &cwd) {
            Ok(request) => request,
            Err(error) => return execution_error_result("Failed to prepare command", error),
        };
        let start = supervisor.start(request);
        tokio::pin!(start);
        let handle = tokio::select! {
            biased;
            () = cancelled.cancelled() => {
                return ToolResult {
                    content: "Command was cancelled before ownership setup completed".to_owned(),
                    is_error: true,
                    images: Vec::new(),
                };
            }
            result = &mut start => match result {
                Ok(handle) => handle,
                Err(error) if Instant::now() >= deadline => {
                    return timeout_error_without_session(timeout_ms, error);
                }
                Err(error) => return execution_error_result("Failed to execute command", error),
            }
        };
        let mut session_guard =
            SessionCancelOnDrop::new(Arc::clone(&supervisor), handle.owner.clone(), handle.session_id);

        let poll = tokio::select! {
            biased;
            () = cancelled.cancelled() => {
                let outcome = supervisor.cancel(&handle.owner, &handle.session_id).await;
                session_guard.disarm();
                return match outcome {
                    Ok(outcome) => render_cancelled(outcome),
                    Err(error) => execution_error_result("Command cancellation failed", error),
                };
            }
            result = supervisor.poll(
                &handle.owner,
                &handle.session_id,
                OutputCursor::START,
                deadline,
            )
            => result
        };
        let poll = match poll {
            Ok(poll) => poll,
            Err(error) => {
                let cleanup = supervisor.cancel(&handle.owner, &handle.session_id).await;
                session_guard.disarm();
                return ToolResult {
                    content: format!(
                        "Command supervision failed: {error}\nCleanup outcome: {}",
                        cleanup
                            .as_ref()
                            .map(format_outcome_summary)
                            .unwrap_or_else(|cleanup_error| cleanup_error.to_string())
                    ),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };

        match poll {
            PollResult::Finished(outcome) => {
                session_guard.disarm();
                render_outcome(outcome)
            }
            PollResult::Running { output, .. } => {
                let outcome = match supervisor.timeout(&handle.owner, &handle.session_id).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        session_guard.disarm();
                        return ToolResult {
                            content: format!(
                                "Command timed out after {timeout_ms}ms.\n{COMMAND_TIMEOUT_GUIDANCE}\n\
                                 Partial output:\n{}\nCleanup failed: {error}",
                                render_output(&output)
                            ),
                            is_error: true,
                            images: Vec::new(),
                        };
                    }
                };
                session_guard.disarm();
                render_timeout(outcome, Some(output), timeout_ms)
            }
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        if cfg!(windows) {
            "Executes a PowerShell command and returns its output. The tool is still named Bash for compatibility, but on Windows the command is run by powershell.exe, not cmd.exe or Unix bash.\n\n\
             IMPORTANT: Do NOT use Bash when a dedicated tool is available:\n\
             - File search: use Glob (not find or ls)\n\
             - Content search: use Grep (not grep or rg)\n\
             - Read files: use Read (not cat, head, or tail)\n\
             - Edit files: use Edit (not sed or awk)\n\
             - Write files: use Write (not echo redirection or cat with heredoc)\n\n\
             # Instructions\n\
             - Use PowerShell syntax: Get-ChildItem, Get-Content, Set-Location, $env:NAME, and ';' for sequencing. Run cmd /C \"...\" explicitly only when cmd.exe syntax is required.\n\
             - Use absolute paths to avoid working directory confusion.\n\
             - When issuing multiple independent commands, make parallel tool calls instead of chaining them. Chain commands only when later commands depend on earlier ones.\n\
             - You may specify an optional timeout in milliseconds (default 120000, max 600000).\n\
             - For installs, dependency downloads, builds, migrations, or other long commands, choose a generous explicit timeout or use exec_command/write_stdin so you can poll instead of killing the command.\n\n\
             # Git safety\n\
             - Never force push, reset --hard, or use --no-verify unless explicitly asked.\n\
             - Prefer creating new commits over amending existing ones."
        } else {
            "Executes a shell command and returns its output.\n\n\
             IMPORTANT: Do NOT use Bash when a dedicated tool is available:\n\
             - File search: use Glob (not find or ls)\n\
             - Content search: use Grep (not grep or rg)\n\
             - Read files: use Read (not cat, head, or tail)\n\
             - Edit files: use Edit (not sed or awk)\n\
             - Write files: use Write (not echo or cat with heredoc)\n\n\
             # Instructions\n\
             - Use absolute paths to avoid working directory confusion.\n\
             - When issuing multiple independent commands, make parallel tool calls instead of chaining them. Use `&&` only when commands depend on each other.\n\
             - You may specify an optional timeout in milliseconds (default 120000, max 600000).\n\
             - For installs, dependency downloads, builds, migrations, or other long commands, choose a generous explicit timeout or use exec_command/write_stdin so you can poll instead of killing the command.\n\n\
             # Git safety\n\
             - Never force push, reset --hard, or use --no-verify unless explicitly asked.\n\
             - Prefer creating new commits over amending existing ones."
        }
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, max 600000)"
                }
            },
            "required": ["command"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(command) = input["command"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: command".to_owned(),
                is_error: true,
                images: Vec::new(),
            };
        };
        let timeout_ms = input["timeout"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        tracing::debug!(cwd = %self.cwd.display(), command, timeout_ms, "BashTool executing");

        let cancelled = CancellationToken::new();
        let mut cancellation_guard = CancelWorkerOnDrop::new(cancelled.clone());
        let worker = tokio::spawn(Self::run_supervised(
            Arc::clone(&self.supervisor),
            self.cwd.clone(),
            self.capability.clone(),
            ExecutionOwner::new(self.run_id, Uuid::now_v7()),
            command.to_owned(),
            timeout_ms,
            cancelled,
        ));
        let result = match worker.await {
            Ok(result) => result,
            Err(error) => ToolResult {
                content: format!("Command worker failed: {error}"),
                is_error: true,
                images: Vec::new(),
            },
        };
        cancellation_guard.disarm();
        result
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        format!("Execute: {}", crate::truncate_utf8(command, 80))
    }
}

struct CancelWorkerOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl CancelWorkerOnDrop {
    fn new(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelWorkerOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

struct SessionCancelOnDrop {
    supervisor: Arc<ProcessSupervisor>,
    owner: ExecutionOwner,
    session_id: nomi_execution::SessionId,
    armed: bool,
}

impl SessionCancelOnDrop {
    fn new(
        supervisor: Arc<ProcessSupervisor>,
        owner: ExecutionOwner,
        session_id: nomi_execution::SessionId,
    ) -> Self {
        Self {
            supervisor,
            owner,
            session_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionCancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let supervisor = Arc::clone(&self.supervisor);
        let owner = self.owner.clone();
        let session_id = self.session_id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = supervisor.cancel(&owner, &session_id).await;
            });
        }
    }
}

fn render_cancelled(outcome: ExecutionOutcome) -> ToolResult {
    let mut result = render_outcome(outcome);
    result.is_error = true;
    result.content = format!("Command was cancelled.\n{}", result.content);
    result
}

fn timeout_error_without_session(timeout_ms: u64, error: ExecutionError) -> ToolResult {
    ToolResult {
        content: format!(
            "Command timed out after {timeout_ms}ms during ownership setup.\n\
             {COMMAND_TIMEOUT_GUIDANCE}\nCleanup result: {error}"
        ),
        is_error: true,
        images: Vec::new(),
    }
}

fn render_timeout(
    outcome: ExecutionOutcome,
    partial: Option<OutputSnapshot>,
    timeout_ms: u64,
) -> ToolResult {
    let (output, summary) = match &outcome {
        ExecutionOutcome::Exited { output, .. }
        | ExecutionOutcome::Cancelled { output, .. }
        | ExecutionOutcome::TimedOut { output, .. } => {
            (Some(output), format_outcome_summary(&outcome))
        }
        ExecutionOutcome::Lost { output, .. } => {
            let output = if output.chunks.is_empty() && output.dropped_bytes == 0 {
                partial.as_ref()
            } else {
                Some(output)
            };
            (output, format_outcome_summary(&outcome))
        }
        ExecutionOutcome::SpawnFailed(_) => {
            (partial.as_ref(), format_outcome_summary(&outcome))
        }
    };
    let mut content = format!(
        "Command timed out after {timeout_ms}ms.\n{COMMAND_TIMEOUT_GUIDANCE}\nPartial output:\n{}",
        output.map(render_output).unwrap_or_else(|| "OUTPUT:\n".to_owned())
    );
    if !summary.is_empty() {
        content.push_str("\nCleanup outcome: ");
        content.push_str(&summary);
    }
    ToolResult {
        content,
        is_error: true,
        images: Vec::new(),
    }
}

fn render_outcome(outcome: ExecutionOutcome) -> ToolResult {
    match outcome {
        ExecutionOutcome::Exited {
            code,
            signal,
            output,
            cleanup,
        } => {
            let exit_code = code.unwrap_or(-1);
            let mut content = format!("Exit code: {exit_code}");
            if let Some(signal) = signal {
                content.push_str(&format!("\nSignal: {signal}"));
            }
            content.push('\n');
            content.push_str(&render_output(&output));
            append_cleanup(&mut content, &cleanup);
            ToolResult {
                content,
                is_error: code != Some(0) || signal.is_some(),
                images: Vec::new(),
            }
        }
        ExecutionOutcome::Cancelled { output, cleanup } => {
            let mut content = format!("Command was cancelled.\n{}", render_output(&output));
            append_cleanup(&mut content, &cleanup);
            ToolResult {
                content,
                is_error: true,
                images: Vec::new(),
            }
        }
        ExecutionOutcome::TimedOut { output, cleanup } => {
            let mut content = format!(
                "Command timed out.\n{COMMAND_TIMEOUT_GUIDANCE}\n{}",
                render_output(&output)
            );
            append_cleanup(&mut content, &cleanup);
            ToolResult {
                content,
                is_error: true,
                images: Vec::new(),
            }
        }
        ExecutionOutcome::Lost {
            last_known,
            output,
            cleanup,
        } => {
            let mut content = format!(
                "Command cleanup is unproven (pid={}, state={:?}). Do not blindly retry.\n{}",
                last_known.pid,
                last_known.state,
                render_output(&output)
            );
            append_cleanup(&mut content, &cleanup);
            ToolResult {
                content,
                is_error: true,
                images: Vec::new(),
            }
        }
        ExecutionOutcome::SpawnFailed(failure) => ToolResult {
            content: format!("Failed to execute command: {} ({})", failure.message, failure.code),
            is_error: true,
            images: Vec::new(),
        },
    }
}

fn render_output(output: &OutputSnapshot) -> String {
    let mut chunks = output.chunks.iter().collect::<Vec<_>>();
    chunks.sort_by_key(|chunk| chunk.seq);
    let mut rendered = String::new();
    let mut current_stream = None;
    for chunk in chunks {
        if current_stream != Some(chunk.stream) {
            if !rendered.is_empty() && !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            rendered.push_str(match chunk.stream {
                OutputStream::Stdout => "STDOUT:\n",
                OutputStream::Stderr => "STDERR:\n",
                OutputStream::Pty => "PTY:\n",
            });
            current_stream = Some(chunk.stream);
        }
        rendered.push_str(&chunk.text);
    }
    if rendered.is_empty() {
        rendered.push_str("OUTPUT:\n");
    }
    if output.dropped_bytes > 0
        || output.encoding.decode_errors > 0
        || output.encoding.source_encoding != "utf-8"
    {
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str(&format!(
            "[output metadata: dropped_bytes={}, source_encoding={}, decode_errors={}]",
            output.dropped_bytes,
            output.encoding.source_encoding,
            output.encoding.decode_errors
        ));
    }
    rendered
}

fn append_cleanup(content: &mut String, cleanup: &CleanupReport) {
    if cleanup.errors.is_empty() {
        return;
    }
    content.push_str("\nCleanup diagnostics: ");
    content.push_str(&cleanup.errors.join("; "));
}

fn format_outcome_summary(outcome: &ExecutionOutcome) -> String {
    match outcome {
        ExecutionOutcome::Exited { code, signal, .. } => {
            format!("exited code={code:?} signal={signal:?}")
        }
        ExecutionOutcome::Cancelled { cleanup, .. } => {
            format!("cancelled reaped={}", cleanup.reaped)
        }
        ExecutionOutcome::TimedOut { cleanup, .. } => {
            format!("timed_out reaped={}", cleanup.reaped)
        }
        ExecutionOutcome::Lost {
            last_known,
            cleanup,
            ..
        } => format!(
            "lost pid={} reaped={} errors={}",
            last_known.pid,
            cleanup.reaped,
            cleanup.errors.join("; ")
        ),
        ExecutionOutcome::SpawnFailed(failure) => {
            format!("spawn_failed {}: {}", failure.code, failure.message)
        }
    }
}

fn execution_error_result(prefix: &str, error: ExecutionError) -> ToolResult {
    ToolResult {
        content: format!("{prefix}: {error} ({})", error.code()),
        is_error: true,
        images: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::test_support::pty_test_helper_bin;
    use crate::test_support::pty_test_helper_shell_cmd;
    use nomi_execution::{SandboxPolicy, SupervisorConfig};
    use serde_json::json;
    use std::path::Path;

    fn tool(cwd: PathBuf) -> BashTool {
        BashTool::new(
            ProcessSupervisor::new(SupervisorConfig::default()),
            cwd.clone(),
            CapabilityPolicy::local_owner(cwd),
        )
    }

    fn shell_quote_path(path: &Path) -> String {
        let path = path.to_string_lossy();
        if cfg!(windows) {
            format!("'{}'", path.replace('\'', "''"))
        } else {
            format!("'{}'", path.replace('\'', "'\"'\"'"))
        }
    }

    async fn wait_for_file(path: &Path, deadline: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < deadline {
            if path.is_file() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        path.is_file()
    }

    fn read_helper_pids(path: &Path) -> Option<(u32, u32)> {
        let content = std::fs::read_to_string(path).ok()?;
        let mut helper = None;
        let mut grandchild = None;
        for line in content.lines() {
            if let Some(value) = line.strip_prefix("helper_pid=") {
                helper = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("grandchild_pid=") {
                grandchild = value.parse().ok();
            }
        }
        Some((helper?, grandchild?))
    }

    #[tokio::test]
    async fn execute_echo_returns_stdout() {
        let result = tool(std::env::temp_dir())
            .execute(json!({"command": "echo hello_bash"}))
            .await;
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains("hello_bash"));
    }

    #[tokio::test]
    async fn execute_invalid_command_returns_error() {
        let result = tool(std::env::temp_dir())
            .execute(json!({"command": "nonexistent_command_xyz_123"}))
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn exit_seven_is_reported_as_an_error() {
        let result = tool(std::env::temp_dir())
            .execute(json!({"command": pty_test_helper_shell_cmd("exit 7")}))
            .await;
        assert!(result.is_error, "exit 7 must be an error: {}", result.content);
        assert!(
            result.content.contains("Exit code: 7"),
            "exact exit code missing: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn timeout_cleans_the_shell_process_and_its_marker_grandchild() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("must-not-appear.marker");
        let ready = directory.path().join("helper-pids.ready");
        let command = pty_test_helper_shell_cmd(&format!(
            "spawn-marker-child 2000 {} {} 60000",
            shell_quote_path(&marker),
            shell_quote_path(&ready)
        ));
        let execution = {
            let tool = tool(directory.path().to_path_buf());
            tokio::spawn(async move {
                tool.execute(json!({"command": command, "timeout": 800}))
                    .await
            })
        };
        let ready_published = wait_for_file(&ready, Duration::from_secs(5)).await;
        let probes = read_helper_pids(&ready).map(|(helper, grandchild)| {
            (ProcessProbe::new(helper), ProcessProbe::new(grandchild))
        });
        let result = tokio::time::timeout(Duration::from_secs(7), execution)
            .await
            .expect("timeout cleanup must remain bounded")
            .expect("Bash execution task should join");
        tokio::time::sleep(Duration::from_millis(1_700)).await;
        let processes_gone = probes
            .as_ref()
            .is_some_and(|(helper, grandchild)| helper.is_gone() && grandchild.is_gone());
        if let Some((helper, grandchild)) = &probes {
            helper.force_kill();
            grandchild.force_kill();
        }
        assert!(result.is_error, "timeout must be an error: {}", result.content);
        assert!(result.content.to_ascii_lowercase().contains("timed out"));
        assert!(ready_published && probes.is_some());
        assert!(!marker.exists(), "grandchild survived timeout");
        assert!(processes_gone, "helper tree survived timeout");
    }

    #[tokio::test]
    async fn timeout_returns_output_flushed_during_interrupt_cleanup() {
        let command = if cfg!(windows) {
            "$null = Register-EngineEvent PowerShell.Exiting -Action { [Console]::Out.WriteLine('cleanup_tail') }; while ($true) { Start-Sleep -Milliseconds 50 }"
        } else {
            "trap 'printf cleanup_tail\\\\n; exit 0' INT; while :; do sleep 1; done"
        };
        let result = tool(std::env::temp_dir())
            .execute(json!({"command": command, "timeout": 500}))
            .await;

        assert!(result.is_error, "timeout must be an error: {}", result.content);
        #[cfg(unix)]
        assert!(
            result.content.contains("cleanup_tail"),
            "terminal cleanup output was omitted: {}",
            result.content
        );
    }

    #[test]
    fn timeout_lost_outcome_prefers_frozen_cleanup_tail() {
        let snapshot = |text: &str| OutputSnapshot {
            chunks: vec![nomi_execution::OutputChunk {
                seq: 1,
                start: 0,
                stream: OutputStream::Stdout,
                bytes: text.as_bytes().to_vec(),
                text: text.to_owned(),
            }],
            next_cursor: OutputCursor::new(text.len() as u64),
            retained_bytes: text.len(),
            dropped_bytes: 0,
            encoding: nomi_execution::EncodingMetadata::default(),
        };
        let now = Instant::now();
        let result = render_timeout(
            ExecutionOutcome::Lost {
                last_known: nomi_execution::ProcessSnapshot {
                    pid: 42,
                    state: nomi_execution::ProcessState::Lost,
                    started_at: now,
                    last_activity_at: now,
                },
                output: snapshot("before-timeout\ncleanup-tail\n"),
                cleanup: CleanupReport::default(),
            },
            Some(snapshot("before-timeout\n")),
            100,
        );

        assert!(result.content.contains("cleanup-tail"), "{}", result.content);
    }

    #[tokio::test]
    async fn execute_respects_cwd() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("cwd_proof.txt"), "proof").unwrap();
        let command = if cfg!(windows) {
            "Get-Content cwd_proof.txt"
        } else {
            "cat cwd_proof.txt"
        };
        let result = tool(directory.path().to_path_buf())
            .execute(json!({"command": command}))
            .await;
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains("proof"));
    }

    #[tokio::test]
    async fn nonexistent_cwd_fails_closed_without_falling_back_to_the_user_profile() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-cwd");
        let marker_name = format!("nomifun-invalid-cwd-{}.marker", Uuid::now_v7());
        let profile_root = std::env::var_os(if cfg!(windows) {
            "USERPROFILE"
        } else {
            "HOME"
        })
        .map(PathBuf::from)
        .expect("the user profile root should be available");
        let marker = profile_root.join(marker_name);
        let _ = std::fs::remove_file(&marker);
        let command = if cfg!(windows) {
            format!(
                "Set-Content -LiteralPath {} -Value must_not_run",
                shell_quote_path(&marker)
            )
        } else {
            format!("printf must_not_run > {}", shell_quote_path(&marker))
        };
        let result = tool(missing)
            .execute(json!({"command": command}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("invalid_working_directory"));
        assert!(
            !marker.exists(),
            "invalid cwd fell back to the user profile and executed the command"
        );
    }

    #[tokio::test]
    async fn deny_execution_capability_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let tool = BashTool::new(
            ProcessSupervisor::new(SupervisorConfig::default()),
            directory.path().to_path_buf(),
            CapabilityPolicy {
                cwd_roots: vec![directory.path().to_path_buf()],
                sandbox: SandboxPolicy::DenyExecution,
                allow_hand_off: false,
            },
        );
        let result = tool.execute(json!({"command": "echo must_not_run"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("capability_denied"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_powershell_redirected_unicode_is_utf8_and_intact() {
        let directory = tempfile::tempdir().unwrap();
        let redirected = directory.path().join("unicode.txt");
        let helper = shell_quote_path(&pty_test_helper_bin());
        let command = format!(
            "& {helper} print-unicode > {}; Get-Content -Raw -Encoding UTF8 {}",
            shell_quote_path(&redirected),
            shell_quote_path(&redirected)
        );
        let result = tool(directory.path().to_path_buf())
            .execute(json!({"command": command}))
            .await;
        assert!(!result.is_error, "unicode command failed: {}", result.content);
        assert!(result.content.contains("中文🙂"), "{}", result.content);
    }

    #[tokio::test]
    async fn aborting_the_bash_caller_cleans_the_helper_and_grandchild() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("abort-must-not-appear.marker");
        let ready = directory.path().join("abort-helper-pids.ready");
        let command = pty_test_helper_shell_cmd(&format!(
            "spawn-marker-child 2000 {} {} 60000",
            shell_quote_path(&marker),
            shell_quote_path(&ready)
        ));
        let execution = {
            let tool = tool(directory.path().to_path_buf());
            tokio::spawn(async move {
                tool.execute(json!({"command": command, "timeout": 30_000}))
                    .await
            })
        };
        assert!(
            wait_for_file(&ready, Duration::from_secs(5)).await,
            "helper never published its PID marker"
        );
        let (helper_pid, grandchild_pid) =
            read_helper_pids(&ready).expect("helper PID marker should be complete");
        let helper = ProcessProbe::new(helper_pid);
        let grandchild = ProcessProbe::new(grandchild_pid);

        execution.abort();
        let _ = execution.await;
        let gone = tokio::time::timeout(Duration::from_secs(7), async {
            loop {
                if helper.is_gone() && grandchild.is_gone() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok();
        tokio::time::sleep(Duration::from_millis(1_700)).await;

        helper.force_kill();
        grandchild.force_kill();
        assert!(gone, "aborting Bash left its supervised process tree alive");
        assert!(!marker.exists(), "grandchild survived the aborted Bash call");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_blocks_writes_outside_the_workspace_root() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let tool = BashTool::new(
            ProcessSupervisor::new(SupervisorConfig::default()),
            canonical.clone(),
            CapabilityPolicy {
                cwd_roots: vec![canonical.clone()],
                sandbox: SandboxPolicy::MacSeatbelt {
                    write_roots: vec![canonical.clone()],
                },
                allow_hand_off: false,
            },
        );
        let inside = canonical.join("inside.txt");
        let allowed = tool
            .execute(json!({"command": format!("echo hi > {}", inside.display())}))
            .await;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert!(inside.exists());

        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let outside = home.join(".nomi_bash_sandbox_escape.txt");
        let _ = std::fs::remove_file(&outside);
        let denied = tool
            .execute(json!({"command": format!("echo hi > {}", outside.display())}))
            .await;
        let escaped = outside.exists();
        let _ = std::fs::remove_file(&outside);
        assert!(denied.is_error);
        assert!(!escaped);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_setup_rejects_a_nonexistent_write_root_without_running_user_code() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let marker = canonical.join("must-not-run.marker");
        let tool = BashTool::new(
            ProcessSupervisor::new(SupervisorConfig::default()),
            canonical.clone(),
            CapabilityPolicy {
                cwd_roots: vec![canonical.clone()],
                sandbox: SandboxPolicy::MacSeatbelt {
                    write_roots: vec![canonical.join("missing-write-root")],
                },
                allow_hand_off: false,
            },
        );

        let result = tool
            .execute(json!({"command": format!("touch {}", shell_quote_path(&marker))}))
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("capability_denied"));
        assert!(!marker.exists(), "sandbox setup failure ran user code");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_rejects_a_write_root_that_is_an_ancestor_of_the_capability_root() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let marker = canonical.join("must-not-run.marker");
        let tool = BashTool::new(
            ProcessSupervisor::new(SupervisorConfig::default()),
            canonical.clone(),
            CapabilityPolicy {
                cwd_roots: vec![canonical.clone()],
                sandbox: SandboxPolicy::MacSeatbelt {
                    write_roots: vec![PathBuf::from("/")],
                },
                allow_hand_off: false,
            },
        );

        let result = tool
            .execute(json!({"command": format!("touch {}", shell_quote_path(&marker))}))
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("capability_denied"));
        assert!(!marker.exists(), "overbroad Seatbelt root ran user code");
    }

    #[cfg(unix)]
    struct ProcessProbe {
        pid: libc::pid_t,
    }

    #[cfg(unix)]
    impl ProcessProbe {
        fn new(pid: u32) -> Self {
            Self {
                pid: pid as libc::pid_t,
            }
        }

        fn is_gone(&self) -> bool {
            if unsafe { libc::kill(self.pid, 0) } == 0 {
                return false;
            }
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }

        fn force_kill(&self) {
            if !self.is_gone() {
                let _ = unsafe { libc::kill(self.pid, libc::SIGKILL) };
            }
        }
    }

    #[cfg(windows)]
    struct ProcessProbe {
        pid: u32,
    }

    #[cfg(windows)]
    impl ProcessProbe {
        fn new(pid: u32) -> Self {
            Self { pid }
        }

        fn is_gone(&self) -> bool {
            let command = format!(
                "if (Get-Process -Id {} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}",
                self.pid
            );
            std::process::Command::new("powershell.exe")
                .args(["-NoLogo", "-NoProfile", "-Command", &command])
                .status()
                .is_ok_and(|status| status.success())
        }

        fn force_kill(&self) {
            if !self.is_gone() {
                let command = format!(
                    "Stop-Process -Id {} -Force -ErrorAction SilentlyContinue",
                    self.pid
                );
                let _ = std::process::Command::new("powershell.exe")
                    .args(["-NoLogo", "-NoProfile", "-Command", &command])
                    .status();
            }
        }
    }

    #[cfg(windows)]
    impl Drop for ProcessProbe {
        fn drop(&mut self) {
            self.force_kill();
        }
    }
}
