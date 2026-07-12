//! Legacy `write_stdin` schema backed by the shared process supervisor.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use nomi_execution::{
    ExecutionError, ExecutionOutcome, PollResult, ProcessSupervisor,
};
use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};
use serde_json::{Value, json};

use crate::{
    Tool,
    exec_command::{render_output, render_terminal_with_missed, transport_label},
    process_store::ProcessStore,
};

const INTERRUPT: &str = "\u{3}";
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const MIN_EMPTY_YIELD_MS: u64 = 5_000;
const MAX_EMPTY_YIELD_MS: u64 = 300_000;

pub struct WriteStdinTool {
    supervisor: Arc<ProcessSupervisor>,
    store: Arc<ProcessStore>,
}

impl WriteStdinTool {
    pub fn new(supervisor: Arc<ProcessSupervisor>, store: Arc<ProcessStore>) -> Self {
        Self { supervisor, store }
    }
}

#[async_trait]
impl Tool for WriteStdinTool {
    fn name(&self) -> &str {
        "write_stdin"
    }

    fn description(&self) -> &str {
        "Writes characters to a live exec_command session and returns incremental output.\n\n\
         - chars defaults to empty, which polls without writing.\n\
         - Send Ctrl-C with chars=\"\\u0003\".\n\
         - To submit a command line to an interactive program, send the line of text in one \
         call, then send the Enter/return key (\"\\r\") as a separate call. A TUI may swallow \
         text plus return sent as one paste burst.\n\
         - If the process has exited, the result reports its exit_code and removes the numeric \
         session adapter."
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
                    "description": "Bytes to write. Empty polls without writing."
                },
                "yield_time_ms": {
                    "type": "number",
                    "description": "Wait time. Writes: default 250/max 30000. Empty poll: default 5000/max 300000."
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
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let chars = input.get("chars").and_then(Value::as_str).unwrap_or("");
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
        let id = match input.get("session_id").and_then(Value::as_u64) {
            Some(id) => id,
            None => {
                return ToolResult::error(
                    "write_stdin: missing required parameter `session_id`",
                );
            }
        };
        let chars = input.get("chars").and_then(Value::as_str).unwrap_or("");
        let entry = match self.store.get(id) {
            Some(entry) => entry,
            None => {
                return ToolResult::error(format!(
                    "write_stdin: unknown or finished session_id={id}"
                ));
            }
        };
        let mut state = entry.lock_state().await;
        if chars == INTERRUPT {
            if let Err(error) = self
                .supervisor
                .interrupt(entry.owner(), &entry.session_id())
                .await
            {
                return recover_action_error(
                    &self.supervisor,
                    &self.store,
                    id,
                    &entry,
                    state,
                    "interrupt",
                    error,
                )
                .await;
            }
        } else if !chars.is_empty()
            && let Err(error) = self
                .supervisor
                .write(entry.owner(), &entry.session_id(), chars.as_bytes())
                .await
        {
            return recover_action_error(
                &self.supervisor,
                &self.store,
                id,
                &entry,
                state,
                "write",
                error,
            )
            .await;
        }

        let yield_ms = requested_yield_ms(&input, chars.is_empty());
        let poll = self
            .supervisor
            .poll_until_activity(
                entry.owner(),
                &entry.session_id(),
                state.cursor(),
                Instant::now() + Duration::from_millis(yield_ms),
            )
            .await;
        let poll = match poll {
            Ok(poll) => poll,
            Err(error) => {
                if matches!(
                    error,
                    ExecutionError::SessionNotFound { .. }
                        | ExecutionError::OwnerMismatch { .. }
                ) {
                    drop(state);
                    self.store.remove_if_same(id, &entry);
                }
                return session_error("poll", id, error);
            }
        };
        match poll {
            PollResult::Running { output, .. } => {
                let observation = state.record_output(&output);
                ToolResult::text(format!(
                    "session_id={id}\ntransport={}\n{}",
                    transport_label(entry.transport()),
                    render_output(&output, Some(observation.missed_bytes))
                ))
            }
            PollResult::Finished(outcome) => {
                let missed_bytes =
                    outcome_output(&outcome).map(|output| state.record_output(output).missed_bytes);
                drop(state);
                self.store.remove_if_same(id, &entry);
                render_terminal_with_missed(outcome, entry.transport(), missed_bytes)
            }
        }
    }
}

fn requested_yield_ms(input: &Value, empty: bool) -> u64 {
    let requested = input.get("yield_time_ms").and_then(Value::as_u64);
    if empty {
        requested
            .unwrap_or(MIN_EMPTY_YIELD_MS)
            .clamp(MIN_EMPTY_YIELD_MS, MAX_EMPTY_YIELD_MS)
    } else {
        requested
            .unwrap_or(MIN_YIELD_MS)
            .clamp(MIN_YIELD_MS, MAX_YIELD_MS)
    }
}

fn outcome_output(outcome: &ExecutionOutcome) -> Option<&nomi_execution::OutputSnapshot> {
    match outcome {
        ExecutionOutcome::Exited { output, .. }
        | ExecutionOutcome::Cancelled { output, .. }
        | ExecutionOutcome::TimedOut { output, .. }
        | ExecutionOutcome::Lost { output, .. } => Some(output),
        ExecutionOutcome::SpawnFailed(_) => None,
    }
}

fn session_error(operation: &str, id: u64, error: ExecutionError) -> ToolResult {
    ToolResult::error(format!(
        "write_stdin: {operation} failed for session_id={id}: {error} ({})",
        error.code()
    ))
}

fn identity_error(error: &ExecutionError) -> bool {
    matches!(
        error,
        ExecutionError::SessionNotFound { .. } | ExecutionError::OwnerMismatch { .. }
    )
}

async fn recover_action_error(
    supervisor: &ProcessSupervisor,
    store: &ProcessStore,
    id: u64,
    entry: &Arc<crate::process_store::LegacySessionEntry>,
    mut state: tokio::sync::MutexGuard<'_, crate::process_store::LegacySessionState>,
    operation: &str,
    error: ExecutionError,
) -> ToolResult {
    if identity_error(&error) {
        drop(state);
        store.remove_if_same(id, entry);
        return session_error(operation, id, error);
    }
    match supervisor
        .poll_until_activity(
            entry.owner(),
            &entry.session_id(),
            state.cursor(),
            Instant::now(),
        )
        .await
    {
        Ok(PollResult::Finished(outcome)) => {
            let missed_bytes =
                outcome_output(&outcome).map(|output| state.record_output(output).missed_bytes);
            drop(state);
            store.remove_if_same(id, entry);
            render_terminal_with_missed(outcome, entry.transport(), missed_bytes)
        }
        Err(identity) if identity_error(&identity) => {
            drop(state);
            store.remove_if_same(id, entry);
            session_error(operation, id, identity)
        }
        Ok(PollResult::Running { .. }) => {
            match supervisor
                .terminal_outcome_if_ready(
                    entry.owner(),
                    &entry.session_id(),
                    state.cursor(),
                ) {
                Ok(Some(outcome)) => {
                    let missed_bytes = outcome_output(&outcome)
                        .map(|output| state.record_output(output).missed_bytes);
                    drop(state);
                    store.remove_if_same(id, entry);
                    render_terminal_with_missed(outcome, entry.transport(), missed_bytes)
                }
                Err(identity) if identity_error(&identity) => {
                    drop(state);
                    store.remove_if_same(id, entry);
                    session_error(operation, id, identity)
                }
                Ok(None) | Err(_) => session_error(operation, id, error),
            }
        }
        Err(_) => session_error(operation, id, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exec_command::ExecCommandTool,
        process_store::LegacySessionBinding,
        test_support::pty_test_helper_shell_cmd,
    };
    use nomi_execution::{
        CapabilityPolicy, ExecutionOwner, OutputCursor, SupervisorConfig, Transport,
    };
    use uuid::Uuid;

    fn tools() -> (ExecCommandTool, WriteStdinTool, Arc<ProcessStore>) {
        let cwd = std::env::current_dir().unwrap();
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let store = Arc::new(ProcessStore::new());
        (
            ExecCommandTool::new(
                Arc::clone(&supervisor),
                Arc::clone(&store),
                cwd.clone(),
                CapabilityPolicy::local_owner(cwd),
            ),
            WriteStdinTool::new(supervisor, Arc::clone(&store)),
            store,
        )
    }

    fn parse_session_id(content: &str) -> Option<u64> {
        content
            .lines()
            .find_map(|line| line.strip_prefix("session_id="))
            .and_then(|value| value.trim().parse().ok())
    }

    #[tokio::test]
    async fn unknown_session_is_error() {
        let (_, writer, _) = tools();
        assert!(
            writer
                .execute(json!({"session_id": 4242}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn string_session_id_is_usable_after_schema_coercion() {
        let (_, writer, _) = tools();
        let input = crate::coerce_input_to_schema(
            &writer.input_schema(),
            json!({"session_id": "4242", "yield_time_ms": "5000"}),
        );
        assert_eq!(input["session_id"].as_u64(), Some(4242));
        assert!(writer.execute(input).await.is_error);
    }

    #[tokio::test]
    async fn missing_session_id_is_error() {
        let (_, writer, _) = tools();
        assert!(writer.execute(json!({"chars": "x"})).await.is_error);
    }

    #[tokio::test]
    async fn writes_and_polls_incremental_output() {
        let (exec, writer, _) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("echo-stdin"),
                "yield_time_ms": 400
            }))
            .await;
        let id = parse_session_id(&started.content)
            .unwrap_or_else(|| panic!("session id: {}", started.content));
        let output = writer
            .execute(json!({
                "session_id": id,
                "chars": "hello_world\n",
                "yield_time_ms": 1500
            }))
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("hello_world"));
    }

    #[tokio::test]
    async fn write_returns_when_output_arrives_instead_of_waiting_full_yield() {
        let (exec, writer, _) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("echo-stdin"),
                "yield_time_ms": 250
            }))
            .await;
        let id = parse_session_id(&started.content)
            .unwrap_or_else(|| panic!("session id: {}", started.content));
        let began = Instant::now();
        let output = writer
            .execute(json!({
                "session_id": id,
                "chars": "fast_echo\n",
                "yield_time_ms": 5000
            }))
            .await;

        assert!(began.elapsed() < Duration::from_secs(1), "{}", output.content);
        assert!(output.content.contains("fast_echo"), "{}", output.content);
    }

    #[tokio::test]
    async fn empty_poll_replays_output_emitted_between_calls() {
        let (exec, writer, _) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("emit-after 300 gap_line 6000"),
                "yield_time_ms": 250
            }))
            .await;
        let id = parse_session_id(&started.content)
            .unwrap_or_else(|| panic!("session id: {}", started.content));
        tokio::time::sleep(Duration::from_millis(700)).await;
        let output = writer
            .execute(json!({"session_id": id, "chars": "", "yield_time_ms": 5000}))
            .await;
        assert!(output.content.contains("gap_line"), "{}", output.content);
    }

    #[tokio::test]
    async fn empty_poll_returns_immediately_after_exit() {
        let (exec, writer, _) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("sleep 600"),
                "yield_time_ms": 250
            }))
            .await;
        let id = parse_session_id(&started.content).expect("session id");
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let began = Instant::now();
        let output = writer
            .execute(json!({"session_id": id, "chars": "", "yield_time_ms": 5000}))
            .await;
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(output.content.contains("exit_code=0"), "{}", output.content);
        assert!(parse_session_id(&output.content).is_none());
    }

    #[tokio::test]
    async fn close_stdin_reaches_truthful_exit_through_the_binding() {
        let (exec, writer, store) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("echo-stdin"),
                "yield_time_ms": 250
            }))
            .await;
        let id = parse_session_id(&started.content).expect("session id");
        let entry = store.get(id).expect("numeric binding");
        writer
            .supervisor
            .write(entry.owner(), &entry.session_id(), b"before-eof\n")
            .await
            .expect("stdin write");
        writer
            .supervisor
            .close_stdin(entry.owner(), &entry.session_id())
            .await
            .expect("pipe stdin close");

        let first = writer
            .execute(json!({
                "session_id": id,
                "chars": "",
                "yield_time_ms": 5000
            }))
            .await;
        let first_content = first.content.clone();
        let result = if parse_session_id(&first.content).is_some() {
            writer
                .execute(json!({
                    "session_id": id,
                    "chars": "",
                    "yield_time_ms": 5000
                }))
                .await
        } else {
            first
        };
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("exit_code=0"), "{}", result.content);
        assert!(
            first_content.contains("before-eof") || result.content.contains("before-eof"),
            "first={}\nresult={}",
            first_content,
            result.content
        );
        assert!(!store.contains(id));
    }

    #[tokio::test]
    async fn write_after_observed_exit_recovers_the_truthful_terminal_outcome() {
        let (exec, writer, store) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("sleep 600"),
                "yield_time_ms": 250
            }))
            .await;
        let id = parse_session_id(&started.content).expect("session id");
        let entry = store.get(id).expect("numeric binding");
        let observed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = writer
                    .supervisor
                    .status(entry.owner(), &entry.session_id())
                    .await
                    .expect("session status");
                if snapshot.state == nomi_execution::ProcessState::Exited {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(observed.is_ok(), "exit was not observed");

        let result = writer
            .execute(json!({
                "session_id": id,
                "chars": "too-late\n",
                "yield_time_ms": 1500
            }))
            .await;
        assert!(
            result.content.contains("exit_code=0")
                && !result.content.contains("write failed"),
            "{}",
            result.content
        );
        assert!(!store.contains(id));
    }

    #[tokio::test]
    async fn wrong_legacy_owner_mapping_is_rejected_and_removed() {
        let cwd = std::env::current_dir().unwrap();
        let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
        let store = Arc::new(ProcessStore::new());
        let owner = ExecutionOwner::new(Uuid::now_v7(), Uuid::now_v7());
        let request = nomi_execution::ExecutionRequest {
            owner: owner.clone(),
            command: nomi_execution::CommandSpec::Shell {
                shell: if cfg!(windows) {
                    nomi_execution::ShellKind::PowerShell
                } else {
                    nomi_execution::ShellKind::Posix
                },
                script: pty_test_helper_shell_cmd("sleep 60000"),
            },
            cwd: cwd.clone(),
            env: Default::default(),
            transport: Transport::Pipe,
            policy: nomi_execution::ExecutionPolicy::default(),
            capability: CapabilityPolicy::local_owner(cwd.clone()),
        };
        let request = nomi_execution::normalize_request(request, &cwd).unwrap();
        let handle = supervisor.start(request).await.unwrap();
        let writer = WriteStdinTool::new(Arc::clone(&supervisor), Arc::clone(&store));
        let id = store
            .insert(LegacySessionBinding::new(
                ExecutionOwner::new(Uuid::now_v7(), Uuid::now_v7()),
                handle.session_id,
                OutputCursor::START,
                0,
                Transport::Pipe,
            ))
            .unwrap();
        let result = writer
            .execute(json!({"session_id": id, "chars": ""}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("owner_mismatch"), "{}", result.content);
        assert!(!store.contains(id));
        let _ = supervisor.cancel(&handle.owner, &handle.session_id).await;
    }

    #[tokio::test]
    async fn expired_session_removes_its_stale_numeric_binding() {
        let cwd = std::env::current_dir().unwrap();
        let supervisor = ProcessSupervisor::new(SupervisorConfig {
            max_sessions: 4,
            reaper_interval: Duration::from_millis(10),
        });
        let store = Arc::new(ProcessStore::new());
        let owner = ExecutionOwner::new(Uuid::now_v7(), Uuid::now_v7());
        let request = nomi_execution::ExecutionRequest {
            owner,
            command: nomi_execution::CommandSpec::Shell {
                shell: if cfg!(windows) {
                    nomi_execution::ShellKind::PowerShell
                } else {
                    nomi_execution::ShellKind::Posix
                },
                script: pty_test_helper_shell_cmd("sleep 60000"),
            },
            cwd: cwd.clone(),
            env: Default::default(),
            transport: Transport::Pipe,
            policy: nomi_execution::ExecutionPolicy {
                lease: Duration::from_millis(120),
                ..nomi_execution::ExecutionPolicy::default()
            },
            capability: CapabilityPolicy::local_owner(cwd.clone()),
        };
        let request = nomi_execution::normalize_request(request, &cwd).unwrap();
        let handle = supervisor.start(request).await.unwrap();
        let id = store
            .insert(LegacySessionBinding::new(
                handle.owner.clone(),
                handle.session_id,
                OutputCursor::START,
                0,
                Transport::Pipe,
            ))
            .unwrap();
        let writer = WriteStdinTool::new(Arc::clone(&supervisor), Arc::clone(&store));

        tokio::time::sleep(Duration::from_millis(500)).await;
        let result = writer
            .execute(json!({"session_id": id, "chars": ""}))
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("session_not_found"),
            "{}",
            result.content
        );
        assert!(!store.contains(id));
    }

    #[tokio::test]
    async fn ctrl_c_uses_supervisor_interrupt() {
        let (exec, writer, store) = tools();
        let started = exec
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("ignore-interrupt"),
                "tty": true,
                "yield_time_ms": 500
            }))
            .await;
        let id = parse_session_id(&started.content)
            .unwrap_or_else(|| panic!("session id: {}", started.content));
        assert!(started.content.contains("ready"), "{}", started.content);
        let result = writer
            .execute(json!({
                "session_id": id,
                "chars": "\u{3}",
                "yield_time_ms": 250
            }))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(parse_session_id(&result.content), Some(id));

        let entry = store.get(id).expect("session remains addressable");
        let outcome = writer
            .supervisor
            .terminate(entry.owner(), &entry.session_id())
            .await
            .expect("terminate after ignored interrupt");
        assert!(matches!(
            outcome,
            ExecutionOutcome::Cancelled { .. } | ExecutionOutcome::Exited { .. }
        ));
        store.remove_if_same(id, &entry);
    }

    #[test]
    fn description_preserves_split_return_guidance() {
        let (_, writer, _) = tools();
        let description = writer.description();

        assert!(description.contains("Enter/return key"));
        assert!(description.contains("\"\\r\") as a separate call"));
        assert!(description.contains("paste burst"));
    }
}
