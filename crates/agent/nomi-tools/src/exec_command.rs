//! `exec_command` legacy command and bounded script schemas backed by the
//! shared process supervisor.
//!
//! Legacy mode keeps the model-visible numeric `session_id` adapter to an
//! owner-qualified UUIDv7 supervisor session. Script mode is one-shot and never
//! enters that adapter. No process or PTY object is retained in this crate.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use nomi_execution::{
    CapabilityPolicy, CommandSpec, ExecutionError, ExecutionOutcome, ExecutionOwner,
    ExecutionPolicy, OutputSnapshot, OutputStream, PollResult, ProcessSupervisor, ShellKind,
    Transport, normalize_request,
};
use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    Tool,
    process_store::{LegacySessionBinding, ProcessStore, missed_bytes},
};

const DEFAULT_YIELD_MS: u64 = 10_000;
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const TERMINAL_SETTLE_MS: u64 = 25;
const MAX_SCRIPT_TIMEOUT_MS: u64 = 600_000;
const SCRIPT_OUTPUT_MAX_BYTES: usize = 48_000;
const PYTHON_PROBE_MAX: Duration = Duration::from_secs(2);
const PYTHON_PROBE_INTERRUPT_GRACE: Duration = Duration::from_millis(25);
const PYTHON_PROBE_TERMINATE_GRACE: Duration = Duration::from_millis(25);
const PYTHON_PROBE_REAP_GRACE: Duration = Duration::from_millis(100);
const PYTHON_PROBE_CLEANUP_BUDGET: Duration = Duration::from_millis(150);
const PTY_COLS: u16 = 120;
const PTY_ROWS: u16 = 30;
const SCRIPT_TIMEOUT_GUIDANCE: &str = "The script was stopped before completion. Do not assume its side effects finished. Inspect the partial state before retrying or running dependent steps.";

struct PreparedInvocation {
    command: PreparedCommand,
    env: BTreeMap<OsString, OsString>,
    transport: Transport,
    mode: InvocationMode,
}

enum PreparedCommand {
    Ready(CommandSpec),
    Python {
        script: String,
        candidates: Vec<PythonCandidate>,
    },
}

struct PythonCandidate {
    program: PathBuf,
    prefix_args: Vec<OsString>,
    display: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PythonProbeWindow {
    execution_deadline: Instant,
    slot_deadline: Instant,
}

impl PythonCandidate {
    fn into_script_command(self, script: String) -> (CommandSpec, String) {
        let mut args = self.prefix_args;
        args.extend([
            OsString::from("-u"),
            OsString::from("-c"),
            OsString::from(script),
        ]);
        (
            CommandSpec::Program {
                program: self.program.into_os_string(),
                args,
            },
            self.display,
        )
    }
}

enum InvocationMode {
    Legacy { yield_ms: u64 },
    Script {
        language: &'static str,
        interpreter: Option<String>,
        timeout_ms: u64,
    },
}

struct ScriptExecutionContext {
    language: &'static str,
    interpreter: String,
    cwd: PathBuf,
    timeout_ms: u64,
    started_at: Instant,
    deadline: Instant,
}

pub struct ExecCommandTool {
    supervisor: Arc<ProcessSupervisor>,
    store: Arc<ProcessStore>,
    default_cwd: PathBuf,
    capability: CapabilityPolicy,
    run_id: Uuid,
}

impl ExecCommandTool {
    pub fn new(
        supervisor: Arc<ProcessSupervisor>,
        store: Arc<ProcessStore>,
        cwd: PathBuf,
        capability: CapabilityPolicy,
    ) -> Self {
        Self {
            supervisor,
            store,
            default_cwd: cwd,
            capability,
            run_id: Uuid::now_v7(),
        }
    }

    async fn execute_legacy_started(
        &self,
        handle: nomi_execution::ExecutionHandle,
        transport: Transport,
        yield_ms: u64,
        mut guard: StartedSessionGuard,
    ) -> ToolResult {
        let poll = self
            .supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                nomi_execution::OutputCursor::START,
                Instant::now() + Duration::from_millis(yield_ms),
            )
            .await;
        let poll = match poll {
            Ok(poll) => poll,
            Err(error) => return execution_error("poll", error),
        };
        match poll {
            PollResult::Finished(outcome) => {
                guard.disarm();
                render_terminal(outcome, transport)
            }
            PollResult::Running {
                output: initial_output,
                ..
            } => {
                let settled = self
                    .supervisor
                    .poll_until_activity(
                        &handle.owner,
                        &handle.session_id,
                        initial_output.next_cursor,
                        Instant::now() + Duration::from_millis(TERMINAL_SETTLE_MS),
                    )
                    .await;
                match settled {
                    Ok(PollResult::Finished(outcome)) => {
                        guard.disarm();
                        return render_terminal(outcome, transport);
                    }
                    Ok(PollResult::Running { .. }) => {}
                    Err(error) => return execution_error("settle poll", error),
                }
                let output = match self
                    .supervisor
                    .poll_until_activity(
                        &handle.owner,
                        &handle.session_id,
                        nomi_execution::OutputCursor::START,
                        Instant::now(),
                    )
                    .await
                {
                    Ok(PollResult::Finished(outcome)) => {
                        guard.disarm();
                        return render_terminal(outcome, transport);
                    }
                    Ok(PollResult::Running { output, .. }) => output,
                    Err(error) => return execution_error("snapshot poll", error),
                };
                let binding = LegacySessionBinding::after_output(
                    handle.owner.clone(),
                    handle.session_id,
                    transport,
                    &output,
                );
                let id = match self.store.insert(binding) {
                    Ok(id) => id,
                    Err(error) => {
                        let cleanup = self
                            .supervisor
                            .terminate(&handle.owner, &handle.session_id)
                            .await;
                        guard.disarm();
                        return ToolResult::error(format!(
                            "exec_command: could not retain the live session: {error}; cleanup={}",
                            cleanup
                                .as_ref()
                                .map(outcome_summary)
                                .unwrap_or_else(|error| error.to_string())
                        ));
                    }
                };
                guard.disarm();
                ToolResult::text(format!(
                    "session_id={id}\ntransport={}\n(process still running — use write_stdin to continue)\n{}",
                    transport_label(transport),
                    render_output(
                        &output,
                        Some(missed_bytes(
                            &output,
                            nomi_execution::OutputCursor::START
                        ))
                    )
                ))
            }
        }
    }

    async fn execute_script_started(
        &self,
        handle: nomi_execution::ExecutionHandle,
        context: ScriptExecutionContext,
        mut guard: StartedSessionGuard,
    ) -> ToolResult {
        let poll = self
            .supervisor
            .poll(
                &handle.owner,
                &handle.session_id,
                nomi_execution::OutputCursor::START,
                context.deadline,
            )
            .await;
        let poll = match poll {
            Ok(poll) => poll,
            Err(error) => {
                let cleanup = self
                    .supervisor
                    .cancel(&handle.owner, &handle.session_id)
                    .await;
                guard.disarm();
                return ToolResult::error(format!(
                    "exec_command: script supervision failed: {error}; cleanup={}",
                    cleanup
                        .as_ref()
                        .map(outcome_summary)
                        .unwrap_or_else(|cleanup_error| cleanup_error.to_string())
                ));
            }
        };

        match poll {
            PollResult::Finished(outcome) => {
                guard.disarm();
                let timed_out = matches!(outcome, ExecutionOutcome::TimedOut { .. });
                let result = render_terminal(outcome, Transport::Pipe);
                let result = if timed_out {
                    add_script_timeout_context(result, context.timeout_ms)
                } else {
                    result
                };
                with_script_summary(
                    result,
                    context.language,
                    &context.interpreter,
                    &context.cwd,
                    context.started_at.elapsed(),
                )
            }
            PollResult::Running { output, .. } => {
                let cleanup = self
                    .supervisor
                    .timeout(&handle.owner, &handle.session_id)
                    .await;
                guard.disarm();
                let mut result = render_script_timeout_cleanup(cleanup, &output);
                result = add_script_timeout_context(result, context.timeout_ms);
                with_script_summary(
                    result,
                    context.language,
                    &context.interpreter,
                    &context.cwd,
                    context.started_at.elapsed(),
                )
            }
        }
    }

    async fn resolve_python(
        &self,
        script: String,
        candidates: Vec<PythonCandidate>,
        cwd: &Path,
        env: &BTreeMap<OsString, OsString>,
        started_at: Instant,
        script_deadline: Instant,
    ) -> Result<(CommandSpec, String), ToolResult> {
        const PROBE_OUTPUT_MAX_BYTES: usize = 4_096;
        const PROBE_MARKER: &str = "NOMI_PYTHON3_OK";
        const PROBE_SOURCE: &str = "import sys; print('NOMI_PYTHON3_OK') if sys.version_info.major == 3 else None; raise SystemExit(0 if sys.version_info.major == 3 else 1)";

        let probe_deadline = started_at
            .checked_add(PYTHON_PROBE_MAX)
            .unwrap_or(started_at)
            .min(script_deadline);
        let candidate_count = candidates.len();
        let mut insufficient_script_budget = false;
        for (index, candidate) in candidates.into_iter().enumerate() {
            let Some(probe_window) = python_probe_candidate_window(
                Instant::now(),
                probe_deadline,
                candidate_count - index,
            ) else {
                insufficient_script_budget = probe_deadline == script_deadline;
                break;
            };
            debug_assert!(probe_window.execution_deadline < probe_window.slot_deadline);
            debug_assert_eq!(
                PYTHON_PROBE_INTERRUPT_GRACE
                    + PYTHON_PROBE_TERMINATE_GRACE
                    + PYTHON_PROBE_REAP_GRACE,
                PYTHON_PROBE_CLEANUP_BUDGET
            );
            let mut probe_args = candidate.prefix_args.clone();
            probe_args.extend([
                OsString::from("-I"),
                OsString::from("-c"),
                OsString::from(PROBE_SOURCE),
            ]);
            let request = nomi_execution::ExecutionRequest {
                owner: ExecutionOwner::new(self.run_id, Uuid::now_v7()),
                command: CommandSpec::Program {
                    program: candidate.program.clone().into_os_string(),
                    args: probe_args,
                },
                cwd: cwd.to_path_buf(),
                env: env.clone(),
                transport: Transport::Pipe,
                policy: ExecutionPolicy {
                    output_limit_bytes: PROBE_OUTPUT_MAX_BYTES,
                    deadline: Some(probe_window.execution_deadline),
                    interrupt_grace: PYTHON_PROBE_INTERRUPT_GRACE,
                    terminate_grace: PYTHON_PROBE_TERMINATE_GRACE,
                    reap_grace: PYTHON_PROBE_REAP_GRACE,
                    ..ExecutionPolicy::default()
                },
                capability: self.capability.clone(),
            };
            let request = match normalize_request(request, &self.default_cwd) {
                Ok(request) => request,
                Err(error) => return Err(execution_error("prepare Python probe", error)),
            };
            let handle = match self.supervisor.start(request).await {
                Ok(handle) => handle,
                Err(ExecutionError::SpawnFailed { .. }) => continue,
                Err(error) => return Err(execution_error("start Python probe", error)),
            };
            let poll = self
                .supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    nomi_execution::OutputCursor::START,
                    probe_window.execution_deadline,
                )
                .await;
            let outcome = match poll {
                Ok(PollResult::Finished(outcome)) => outcome,
                Ok(PollResult::Running { .. }) => self
                    .supervisor
                    .timeout(&handle.owner, &handle.session_id)
                    .await
                    .map_err(|error| execution_error("stop Python probe", error))?,
                Err(error) => {
                    let cleanup = self
                        .supervisor
                        .cancel(&handle.owner, &handle.session_id)
                        .await;
                    return Err(ToolResult::error(format!(
                        "exec_command: Python probe supervision failed: {error}; cleanup={}",
                        cleanup
                            .as_ref()
                            .map(outcome_summary)
                            .unwrap_or_else(|cleanup_error| cleanup_error.to_string())
                    )));
                }
            };
            if matches!(
                &outcome,
                ExecutionOutcome::Exited {
                    code: Some(0),
                    output,
                    ..
                } if output.text().lines().any(|line| line.trim() == PROBE_MARKER)
            ) {
                return Ok(candidate.into_script_command(script));
            }
            if matches!(
                &outcome,
                ExecutionOutcome::Lost {
                    cleanup,
                    ..
                } if !cleanup.reaped
            ) {
                return Err(ToolResult::error(format!(
                    "exec_command: Python probe cleanup is unproven: {}",
                    outcome_summary(&outcome)
                )));
            }
        }

        if insufficient_script_budget {
            Err(ToolResult::error(format!(
                "exec_command: script timeout is too short for supervised Python interpreter validation\n{SCRIPT_TIMEOUT_GUIDANCE}"
            )))
        } else if Instant::now() >= script_deadline {
            Err(ToolResult::error(format!(
                "exec_command: script timed out during Python interpreter validation\n{SCRIPT_TIMEOUT_GUIDANCE}"
            )))
        } else {
            Err(ToolResult::error(
                "exec_command: python_unavailable: script mode requires a runnable host-provided Python 3 interpreter",
            ))
        }
    }
}

fn python_probe_candidate_window(
    now: Instant,
    overall_deadline: Instant,
    candidates_left: usize,
) -> Option<PythonProbeWindow> {
    if candidates_left == 0 || now >= overall_deadline {
        return None;
    }
    let share = overall_deadline.duration_since(now) / candidates_left as u32;
    let execution_budget = share.checked_sub(PYTHON_PROBE_CLEANUP_BUDGET)?;
    if execution_budget.is_zero() {
        return None;
    }
    Some(PythonProbeWindow {
        execution_deadline: now.checked_add(execution_budget)?,
        slot_deadline: now.checked_add(share)?.min(overall_deadline),
    })
}

fn add_script_timeout_context(mut result: ToolResult, timeout_ms: u64) -> ToolResult {
    result.is_error = true;
    result.content = format!(
        "Script timed out after {timeout_ms}ms.\n{SCRIPT_TIMEOUT_GUIDANCE}\n{}",
        result.content
    );
    result
}

fn render_script_timeout_cleanup(
    cleanup: Result<ExecutionOutcome, ExecutionError>,
    captured: &OutputSnapshot,
) -> ToolResult {
    match cleanup {
        Ok(outcome) => {
            let lost_output_is_empty = matches!(
                &outcome,
                ExecutionOutcome::Lost { output, .. }
                    if output.chunks.is_empty() && output.dropped_bytes == 0
            );
            let mut result = render_terminal(outcome, Transport::Pipe);
            if lost_output_is_empty
                && (!captured.chunks.is_empty() || captured.dropped_bytes > 0)
            {
                result.content = format!(
                    "Partial output captured before cleanup:\n{}\n{}",
                    render_output(captured, None),
                    result.content
                );
            }
            result
        }
        Err(error) => ToolResult::error(format!(
            "Partial output:\n{}\nCleanup failed: {error}",
            render_output(captured, None)
        )),
    }
}

#[async_trait]
impl Tool for ExecCommandTool {
    fn name(&self) -> &str {
        "exec_command"
    }

    fn description(&self) -> &str {
        "Runs either one shell command or one bounded, non-interactive shell/Python script through \
         the shared process supervisor. Legacy commands may return a numeric session_id for ongoing interaction.\n\n\
         In legacy cmd mode, the command is executed by the platform shell. On Windows this is PowerShell \
         (use PowerShell syntax such as Get-ChildItem, $env:NAME, and ';' for sequencing; \
         run cmd /C \"...\" explicitly when cmd.exe syntax is required). On macOS/Linux this \
         is POSIX sh.\n\n\
         Script mode requires script, language (shell or python), and a hard timeout in milliseconds. \
         It is for deterministic, homogeneous local batches that need no intermediate model decision \
         or approval. It always uses pipe transport, never returns a live session, and does not download \
         Python when the host has no Python 3 interpreter. Validate preconditions, fail non-zero on a \
         dependent-operation failure, bound output, and print a concise final summary. Do not use scripts \
         to bypass dedicated file, browser, UI, MCP, or approval-aware tools.\n\n\
         Use tty=true for REPLs, TUIs, and interactive installers.\n\n\
         - tty=false uses separate stdout/stderr pipe streams.\n\
         - tty=true uses a merged PTY stream for interactive programs.\n\
         - If the process exits within yield_time_ms, the result reports its exit_code and no \
         session_id.\n\
         - If it remains live, use write_stdin with the returned session_id.\n\n\
         IMPORTANT (TUI submit): send the line of text first, then send the Enter/return key \
         (\"\\r\") as its own write_stdin call. A TUI may treat text plus return in one burst as \
         pasted input and leave the command unsubmitted."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "Legacy shell command. Mutually exclusive with script."
                },
                "script": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Literal non-interactive script source. Mutually exclusive with cmd."
                },
                "language": {
                    "type": "string",
                    "enum": ["shell", "python"],
                    "description": "Script language. Required with script and forbidden with cmd."
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory. Defaults to the session cwd."
                },
                "tty": {
                    "type": "boolean",
                    "description": "Use PTY transport. Defaults to false (pipe)."
                },
                "yield_time_ms": {
                    "type": "number",
                    "description": "Legacy cmd mode only. Milliseconds to wait before yielding. Default 10000, range 250-30000."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SCRIPT_TIMEOUT_MS,
                    "description": "Script mode only. Hard execution deadline in milliseconds."
                }
            },
            "oneOf": [
                {
                    "required": ["cmd"],
                    "not": {
                        "anyOf": [
                            { "required": ["script"] },
                            { "required": ["language"] },
                            { "required": ["timeout"] }
                        ]
                    }
                },
                {
                    "required": ["script", "language", "timeout"],
                    "not": {
                        "anyOf": [
                            { "required": ["cmd"] },
                            { "required": ["tty"] },
                            { "required": ["yield_time_ms"] }
                        ]
                    }
                }
            ],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        if let Some(script) = input.get("script").and_then(Value::as_str) {
            let language = input
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return format!(
                "exec_command script ({language}): {}",
                crate::truncate_utf8(script, 80)
            );
        }
        let command = input.get("cmd").and_then(Value::as_str).unwrap_or("");
        format!("exec_command: {}", crate::truncate_utf8(command, 80))
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let invocation = match requested_invocation(&input) {
            Ok(invocation) => invocation,
            Err(error) => return ToolResult::error(format!("exec_command: {error}")),
        };
        let cwd = match requested_workdir(&input, &self.default_cwd) {
            Ok(cwd) => cwd,
            Err(error) => return ToolResult::error(format!("exec_command: {error}")),
        };
        let PreparedInvocation {
            command,
            env,
            transport,
            mode,
        } = invocation;
        let mut mode = mode;
        let started_at = Instant::now();
        let deadline = match &mode {
            InvocationMode::Legacy { .. } => None,
            InvocationMode::Script { timeout_ms, .. } => Some(
                started_at
                    .checked_add(Duration::from_millis(*timeout_ms))
                    .unwrap_or(started_at),
            ),
        };
        prune_stale_bindings(&self.supervisor, &self.store);
        let command = match command {
            PreparedCommand::Ready(command) => command,
            PreparedCommand::Python { script, candidates } => {
                let script_deadline = deadline.expect("Python script mode always has a deadline");
                let (command, resolved_interpreter) = match self
                    .resolve_python(
                        script,
                        candidates,
                        &cwd,
                        &env,
                        started_at,
                        script_deadline,
                    )
                    .await
                {
                    Ok(resolved) => resolved,
                    Err(error) => return error,
                };
                let InvocationMode::Script { interpreter, .. } = &mut mode else {
                    unreachable!("deferred Python command is script-only");
                };
                *interpreter = Some(resolved_interpreter);
                command
            }
        };
        let owner = ExecutionOwner::new(self.run_id, Uuid::now_v7());
        let request = nomi_execution::ExecutionRequest {
            owner,
            command,
            cwd: cwd.clone(),
            env,
            transport,
            policy: ExecutionPolicy {
                output_limit_bytes: if deadline.is_some() {
                    SCRIPT_OUTPUT_MAX_BYTES
                } else {
                    ExecutionPolicy::default().output_limit_bytes
                },
                deadline,
                ..ExecutionPolicy::default()
            },
            capability: self.capability.clone(),
        };
        let request = match normalize_request(request, &self.default_cwd) {
            Ok(request) => request,
            Err(error) => return execution_error("prepare", error),
        };
        let handle = match self.supervisor.start(request).await {
            Ok(handle) => handle,
            Err(error) => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return ToolResult::error(format!(
                        "exec_command: script timed out during ownership setup: {error}\n{SCRIPT_TIMEOUT_GUIDANCE}"
                    ));
                }
                return execution_error("start", error);
            }
        };
        let guard = StartedSessionGuard::new(
            Arc::clone(&self.supervisor),
            handle.owner.clone(),
            handle.session_id,
        );
        match mode {
            InvocationMode::Legacy { yield_ms } => {
                self.execute_legacy_started(handle, transport, yield_ms, guard)
                    .await
            }
            InvocationMode::Script {
                language,
                interpreter,
                timeout_ms,
            } => {
                let context = ScriptExecutionContext {
                    language,
                    interpreter: interpreter
                        .expect("script interpreter must resolve before execution"),
                    cwd,
                    timeout_ms,
                    started_at,
                    deadline: deadline.expect("script mode always has a deadline"),
                };
                self.execute_script_started(handle, context, guard).await
            }
        }
    }
}

fn requested_invocation(input: &Value) -> Result<PreparedInvocation, String> {
    if !input.is_object() {
        return Err("input must be an object".to_string());
    }
    let has_command = input.get("cmd").is_some();
    let has_script = input.get("script").is_some();
    if has_command == has_script {
        return Err("provide exactly one of `cmd` or `script`".to_string());
    }

    if has_command {
        reject_unknown_fields(input, &["cmd", "workdir", "tty", "yield_time_ms"])?;
        if input.get("language").is_some() || input.get("timeout").is_some() {
            return Err("language and timeout are only valid with script mode".to_string());
        }
        let command = input
            .get("cmd")
            .and_then(Value::as_str)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| "cmd must be a non-empty string".to_string())?
            .to_owned();
        let tty = input.get("tty").and_then(Value::as_bool).unwrap_or(false);
        let transport = if tty {
            Transport::Pty {
                cols: PTY_COLS,
                rows: PTY_ROWS,
            }
        } else {
            Transport::Pipe
        };
        let yield_ms = input
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_YIELD_MS)
            .clamp(MIN_YIELD_MS, MAX_YIELD_MS);
        return Ok(PreparedInvocation {
            command: PreparedCommand::Ready(CommandSpec::Shell {
                shell: if cfg!(windows) {
                    ShellKind::PowerShell
                } else {
                    ShellKind::Posix
                },
                script: command,
            }),
            env: BTreeMap::new(),
            transport,
            mode: InvocationMode::Legacy { yield_ms },
        });
    }

    reject_unknown_fields(input, &["script", "language", "timeout", "workdir"])?;
    if input.get("tty").is_some() || input.get("yield_time_ms").is_some() {
        return Err("script mode is non-interactive and forbids tty and yield_time_ms".to_string());
    }
    if input.get("workdir").is_some_and(|workdir| !workdir.is_string()) {
        return Err("script workdir must be a string when provided".to_string());
    }
    let script = input
        .get("script")
        .and_then(Value::as_str)
        .filter(|script| !script.trim().is_empty())
        .ok_or_else(|| "script must be a non-empty string".to_string())?
        .to_owned();
    let timeout_ms = input
        .get("timeout")
        .and_then(Value::as_u64)
        .filter(|timeout| (1..=MAX_SCRIPT_TIMEOUT_MS).contains(timeout))
        .ok_or_else(|| {
            format!(
                "script timeout must be an integer from 1 to {MAX_SCRIPT_TIMEOUT_MS} milliseconds"
            )
        })?;
    let language = input
        .get("language")
        .and_then(Value::as_str)
        .ok_or_else(|| "script mode requires language=shell or language=python".to_string())?;

    let (command, interpreter, env, language) = match language {
        "shell" => (
            PreparedCommand::Ready(CommandSpec::Shell {
                shell: if cfg!(windows) {
                    ShellKind::PowerShellLiteral
                } else {
                    ShellKind::Posix
                },
                script,
            }),
            Some(if cfg!(windows) {
                "PowerShell".to_string()
            } else {
                "/bin/sh".to_string()
            }),
            BTreeMap::new(),
            "shell",
        ),
        "python" => {
            let (command, interpreter) = prepare_python_command(script)?;
            (command, interpreter, python_environment(), "python")
        }
        other => {
            return Err(format!(
                "unsupported script language `{other}`; expected shell or python"
            ));
        }
    };

    Ok(PreparedInvocation {
        command,
        env,
        transport: Transport::Pipe,
        mode: InvocationMode::Script {
            language,
            interpreter,
            timeout_ms,
        },
    })
}

fn reject_unknown_fields(input: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = input
        .as_object()
        .ok_or_else(|| "input must be an object".to_string())?;
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!(
            "unsupported field `{field}`; allowed fields: {}",
            allowed.join(", ")
        ));
    }
    Ok(())
}

fn python_environment() -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (OsString::from("PYTHONUTF8"), OsString::from("1")),
        (
            OsString::from("PYTHONIOENCODING"),
            OsString::from("utf-8"),
        ),
    ])
}

#[cfg(not(windows))]
fn prepare_python_command(script: String) -> Result<(PreparedCommand, Option<String>), String> {
    prepare_unix_python_command(script, which::which("python3").ok())
}

#[cfg(not(windows))]
fn prepare_unix_python_command(
    script: String,
    program: Option<PathBuf>,
) -> Result<(PreparedCommand, Option<String>), String> {
    let program = program.ok_or_else(|| {
        "python_unavailable: script mode requires a host-provided Python 3 interpreter".to_string()
    })?;
    let display = program.display().to_string();
    Ok((
        PreparedCommand::Python {
            script,
            candidates: vec![PythonCandidate {
                program,
                prefix_args: Vec::new(),
                display,
            }],
        },
        None,
    ))
}

#[cfg(windows)]
fn prepare_python_command(script: String) -> Result<(PreparedCommand, Option<String>), String> {
    let mut candidates = Vec::new();
    for candidate in ["py", "python3", "python"] {
        let Ok(program) = which::which(candidate) else {
            continue;
        };
        let mut prefix_args = Vec::new();
        if candidate == "py" {
            prefix_args.push(OsString::from("-3"));
        }
        let display = if candidate == "py" {
            format!("{} -3", program.display())
        } else {
            program.display().to_string()
        };
        candidates.push(PythonCandidate {
            program,
            prefix_args,
            display,
        });
    }
    if candidates.is_empty() {
        return Err(
            "python_unavailable: script mode requires a host-provided Python 3 interpreter"
                .to_string(),
        );
    }
    Ok((
        PreparedCommand::Python { script, candidates },
        None,
    ))
}

fn with_script_summary(
    mut result: ToolResult,
    language: &str,
    interpreter: &str,
    cwd: &Path,
    elapsed: Duration,
) -> ToolResult {
    result.content = format!(
        "mode=script\nlanguage={language}\ninterpreter={interpreter}\nworkdir={}\nelapsed_ms={}\n{}",
        cwd.display(),
        elapsed.as_millis(),
        result.content
    );
    result
}

fn prune_stale_bindings(supervisor: &ProcessSupervisor, store: &ProcessStore) {
    for (id, entry) in store.entries() {
        // A ready terminal outcome may still contain unread tail output for the
        // next write_stdin poll, so retain that mapping. Only identities the
        // supervisor has already retired (or cannot authenticate) are stale.
        match supervisor
            .terminal_outcome_if_ready(
                entry.owner(),
                &entry.session_id(),
                nomi_execution::OutputCursor::START,
            )
        {
            Err(ExecutionError::SessionNotFound { .. })
            | Err(ExecutionError::OwnerMismatch { .. }) => {
                store.remove_if_same(id, &entry);
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {}
        }
    }
}

fn requested_workdir(input: &Value, default: &Path) -> Result<PathBuf, &'static str> {
    match input.get("workdir") {
        None | Some(Value::Null) => Ok(default.to_path_buf()),
        Some(Value::String(value)) if value.is_empty() => Err("workdir must not be empty"),
        Some(Value::String(value)) => Ok(PathBuf::from(value)),
        Some(_) => Err("workdir must be a string"),
    }
}

pub(crate) fn render_output(
    output: &OutputSnapshot,
    missed_bytes: Option<u64>,
) -> String {
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
    let missed_bytes = missed_bytes.unwrap_or(0);
    if missed_bytes > 0
        || output.dropped_bytes > 0
        || output.encoding.decode_errors > 0
        || output.encoding.source_encoding != "utf-8"
    {
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str(&format!(
            "[output metadata: missed_bytes={missed_bytes}, dropped_bytes={}, source_encoding={}, decode_errors={}]",
            output.dropped_bytes,
            output.encoding.source_encoding,
            output.encoding.decode_errors
        ));
    }
    rendered
}

pub(crate) fn render_terminal(
    outcome: ExecutionOutcome,
    transport: Transport,
) -> ToolResult {
    render_terminal_with_missed(outcome, transport, None)
}

pub(crate) fn render_terminal_with_missed(
    outcome: ExecutionOutcome,
    transport: Transport,
    missed_bytes: Option<u64>,
) -> ToolResult {
    match outcome {
        ExecutionOutcome::Exited {
            code,
            signal,
            output,
            cleanup,
        } => {
            let exit_code = code.unwrap_or(-1);
            let mut content = format!(
                "(process exited, exit_code={exit_code})\ntransport={}\n{}",
                transport_label(transport),
                render_output(&output, missed_bytes)
            );
            if let Some(signal) = signal {
                content.push_str(&format!("\nsignal={signal}"));
            }
            append_cleanup(&mut content, &cleanup);
            ToolResult {
                content,
                is_error: code != Some(0) || signal.is_some(),
                images: Vec::new(),
            }
        }
        ExecutionOutcome::Cancelled { output, cleanup } => {
            let mut content = format!(
                "(process cancelled)\ntransport={}\n{}",
                transport_label(transport),
                render_output(&output, missed_bytes)
            );
            append_cleanup(&mut content, &cleanup);
            ToolResult::error(content)
        }
        ExecutionOutcome::TimedOut { output, cleanup } => {
            let mut content = format!(
                "(process timed out)\ntransport={}\n{}",
                transport_label(transport),
                render_output(&output, missed_bytes)
            );
            append_cleanup(&mut content, &cleanup);
            ToolResult::error(content)
        }
        ExecutionOutcome::Lost {
            last_known,
            output,
            cleanup,
        } => {
            let mut content = format!(
                "(process lost, pid={}, state={:?})\ntransport={}\n{}",
                last_known.pid,
                last_known.state,
                transport_label(transport),
                render_output(&output, missed_bytes)
            );
            append_cleanup(&mut content, &cleanup);
            ToolResult::error(content)
        }
        ExecutionOutcome::SpawnFailed(failure) => ToolResult::error(format!(
            "exec_command: spawn failed: {} ({})",
            failure.message, failure.code
        )),
    }
}

pub(crate) fn transport_label(transport: Transport) -> &'static str {
    match transport {
        Transport::Pipe => "pipe",
        Transport::Pty { .. } => "pty",
    }
}

pub(crate) fn outcome_summary(outcome: &ExecutionOutcome) -> String {
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

fn append_cleanup(content: &mut String, cleanup: &nomi_execution::CleanupReport) {
    if !cleanup.errors.is_empty() {
        content.push_str("\ncleanup diagnostics: ");
        content.push_str(&cleanup.errors.join("; "));
    }
}

fn execution_error(operation: &str, error: ExecutionError) -> ToolResult {
    ToolResult::error(format!(
        "exec_command: {operation} failed: {error} ({})",
        error.code()
    ))
}

struct StartedSessionGuard {
    supervisor: Arc<ProcessSupervisor>,
    owner: ExecutionOwner,
    session_id: nomi_execution::SessionId,
    armed: bool,
}

impl StartedSessionGuard {
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

impl Drop for StartedSessionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let supervisor = Arc::clone(&self.supervisor);
        let owner = self.owner.clone();
        let session_id = self.session_id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = supervisor.terminate(&owner, &session_id).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pty_test_helper_shell_cmd;

    fn tool(cwd: PathBuf) -> (ExecCommandTool, Arc<ProcessStore>) {
        let supervisor = ProcessSupervisor::new(nomi_execution::SupervisorConfig::default());
        let store = Arc::new(ProcessStore::new());
        (
            ExecCommandTool::new(
                supervisor,
                Arc::clone(&store),
                cwd.clone(),
                CapabilityPolicy::local_owner(cwd),
            ),
            store,
        )
    }

    fn parse_session_id(content: &str) -> Option<u64> {
        content
            .lines()
            .find_map(|line| line.strip_prefix("session_id="))
            .and_then(|value| value.trim().parse().ok())
    }

    fn stdout_stderr_command() -> &'static str {
        if cfg!(windows) {
            "[Console]::Out.WriteLine('pipe_stdout_marker'); [Console]::Error.WriteLine('pipe_stderr_marker')"
        } else {
            "printf 'pipe_stdout_marker\\n'; printf 'pipe_stderr_marker\\n' >&2"
        }
    }

    fn assert_marker_stream(content: &str, marker: &str, expected: &str) {
        let marker_index = content.find(marker).expect("marker");
        let prefix = &content[..marker_index];
        let stdout = prefix.rfind("STDOUT:\n");
        let stderr = prefix.rfind("STDERR:\n");
        let actual = match (stdout, stderr) {
            (Some(left), Some(right)) if left > right => "STDOUT:\n",
            (Some(_), Some(_)) => "STDERR:\n",
            (Some(_), None) => "STDOUT:\n",
            (None, Some(_)) => "STDERR:\n",
            (None, None) => panic!("missing stream label: {content}"),
        };
        assert_eq!(actual, expected);
    }

    async fn execute_in_workdir(default_cwd: PathBuf, workdir: &str) -> ToolResult {
        tool(default_cwd)
            .0
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("exit 0"),
                "workdir": workdir,
                "tty": false,
                "yield_time_ms": 250
            }))
            .await
    }

    #[tokio::test]
    async fn immediate_exit_reports_exit_code_no_session() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("exit 0"),
                "yield_time_ms": 3000
            }))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("exit_code=0"));
        assert!(parse_session_id(&result.content).is_none());
    }

    #[tokio::test]
    async fn immediate_exit_does_not_wait_for_the_remaining_yield() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let started = Instant::now();
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("exit 0"),
                "yield_time_ms": 3000
            }))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn tty_false_quick_command_keeps_both_streams_through_terminal_exit() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": stdout_stderr_command(),
                "tty": false,
                "yield_time_ms": 10_000
            }))
            .await;

        assert!(
            !result.is_error && parse_session_id(&result.content).is_none(),
            "{}",
            result.content
        );
        assert_marker_stream(&result.content, "pipe_stdout_marker", "STDOUT:\n");
        assert_marker_stream(&result.content, "pipe_stderr_marker", "STDERR:\n");
    }

    #[tokio::test]
    async fn tty_true_reports_pty_transport() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("exit 0"),
                "tty": true,
                "yield_time_ms": 1000
            }))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("transport=pty"));
    }

    #[tokio::test]
    async fn exit_seven_is_an_error() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("exit 7"),
                "yield_time_ms": 3000
            }))
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("exit_code=7"), "{}", result.content);
    }

    #[tokio::test]
    async fn long_lived_returns_a_numeric_adapter_session() {
        let (tool, store) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("echo-stdin"),
                "yield_time_ms": 400
            }))
            .await;
        let id = parse_session_id(&result.content).expect("session id");
        assert!(store.contains(id));
    }

    #[tokio::test]
    async fn settle_poll_preserves_output_from_before_and_during_settle() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd(
                    "emit-twice 200 first_marker 20 second_marker 60000"
                ),
                "yield_time_ms": 250
            }))
            .await;

        assert!(result.content.contains("first_marker"), "{}", result.content);
        assert!(result.content.contains("second_marker"), "{}", result.content);
    }

    #[tokio::test]
    async fn output_before_exit_still_returns_terminal_within_the_initial_yield() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "cmd": pty_test_helper_shell_cmd("emit-after 50 before_exit 300"),
                "yield_time_ms": 1000
            }))
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("before_exit"), "{}", result.content);
        assert!(result.content.contains("exit_code=0"), "{}", result.content);
        assert!(parse_session_id(&result.content).is_none());
    }

    #[test]
    fn initial_output_renderer_reports_exact_missed_bytes() {
        let output = OutputSnapshot {
            chunks: Vec::new(),
            next_cursor: nomi_execution::OutputCursor::new(4 * 1024 * 1024 + 17),
            retained_bytes: 4 * 1024 * 1024,
            dropped_bytes: 17,
            encoding: nomi_execution::EncodingMetadata::default(),
        };
        let rendered = render_output(
            &output,
            Some(missed_bytes(
                &output,
                nomi_execution::OutputCursor::START,
            )),
        );

        assert!(rendered.contains("missed_bytes=17"), "{rendered}");
        assert!(rendered.contains("dropped_bytes=17"), "{rendered}");
    }

    #[tokio::test]
    async fn invalid_workdirs_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let file = root.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let outside = tempfile::tempdir().unwrap();
        for workdir in [
            "",
            missing.to_str().unwrap(),
            file.to_str().unwrap(),
            outside.path().to_str().unwrap(),
        ] {
            let result = execute_in_workdir(root.path().to_path_buf(), workdir).await;
            assert!(result.is_error, "workdir={workdir}: {}", result.content);
        }
    }

    #[tokio::test]
    async fn missing_cmd_is_error() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        assert!(tool.execute(json!({})).await.is_error);
    }

    #[test]
    fn schema_keeps_legacy_command_and_adds_strict_script_mode() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let schema = tool.input_schema();
        let properties = schema["properties"].as_object().unwrap();

        assert!(properties.contains_key("cmd"));
        assert_eq!(properties["language"]["enum"], json!(["shell", "python"]));
        assert_eq!(properties["script"]["type"], "string");
        assert_eq!(properties["timeout"]["maximum"], 600_000);
        assert!(schema.get("oneOf").is_some());
        assert_eq!(schema["additionalProperties"], false);
    }

    #[tokio::test]
    async fn script_mode_rejects_ambiguous_or_interactive_inputs_before_spawn() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        for input in [
            json!({ "cmd": "exit 0", "script": "exit 0", "language": "shell", "timeout": 1000 }),
            json!({ "script": "exit 0", "timeout": 1000 }),
            json!({ "script": "exit 0", "language": "unknown", "timeout": 1000 }),
            json!({ "script": "   ", "language": "shell", "timeout": 1000 }),
            json!({ "script": "exit 0", "language": "shell" }),
            json!({ "script": "exit 0", "language": "shell", "timeout": 1000, "tty": true }),
            json!({ "script": "exit 0", "language": "shell", "timeout": 1000, "yield_time_ms": 250 }),
            json!({ "script": "exit 0", "language": "shell", "timeout": 1000, "unknown": true }),
            json!({ "script": "exit 0", "language": "shell", "timeout": 1000, "workdir": null }),
            json!({ "cmd": "exit 0", "language": "shell" }),
            json!({ "cmd": "exit 0", "unknown": true }),
        ] {
            let result = tool.execute(input).await;
            assert!(result.is_error, "input should fail closed: {}", result.content);
            assert!(parse_session_id(&result.content).is_none());
        }
    }

    #[tokio::test]
    async fn shell_script_mode_runs_multiline_batch_to_terminal() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let script = if cfg!(windows) {
            "[Console]::Out.WriteLine('script-first')\n[Console]::Out.WriteLine('script-second')"
        } else {
            "printf 'script-first\\n'\nprintf 'script-second\\n'"
        };
        let result = tool
            .execute(json!({
                "script": script,
                "language": "shell",
                "timeout": 3000
            }))
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("mode=script"));
        assert!(result.content.contains("language=shell"));
        assert!(result.content.contains("script-first"));
        assert!(result.content.contains("script-second"));
        assert!(parse_session_id(&result.content).is_none());
    }

    #[tokio::test]
    async fn python_script_mode_is_direct_and_utf8_stable() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let result = tool
            .execute(json!({
                "script": "print('中文🙂 $() `literal` ; \\\\')",
                "language": "python",
                "timeout": 3000
            }))
            .await;

        if result.is_error && result.content.contains("python_unavailable") {
            return;
        }
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("language=python"));
        assert!(result.content.contains("中文🙂 $() `literal` ; \\"));
        assert!(parse_session_id(&result.content).is_none());
    }

    #[tokio::test]
    async fn script_timeout_cancels_instead_of_returning_a_live_session() {
        let (tool, store) = tool(std::env::current_dir().unwrap());
        let script = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let result = tool
            .execute(json!({
                "script": script,
                "language": "shell",
                "timeout": 100
            }))
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("timed out"), "{}", result.content);
        assert!(
            result.content.contains("Do not assume its side effects finished"),
            "{}",
            result.content
        );
        assert!(parse_session_id(&result.content).is_none());
        assert!(store.is_empty(), "script mode must never retain a live session");
    }

    #[cfg(not(windows))]
    #[test]
    fn python_script_maps_to_direct_program_execution() {
        let Ok(prepared) = requested_invocation(&json!({
            "script": "print('literal')",
            "language": "python",
            "timeout": 1000
        })) else {
            return;
        };

        let PreparedCommand::Python {
            script,
            mut candidates,
        } = prepared.command
        else {
            panic!("python source must not be interpolated into a shell command");
        };
        let (command, _) = candidates
            .pop()
            .expect("resolved Python candidate")
            .into_script_command(script);
        let CommandSpec::Program { args, .. } = command else {
            unreachable!();
        };
        assert!(args.iter().any(|arg| arg == "-c"));
        assert!(args.iter().any(|arg| arg == "print('literal')"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unavailable_unix_python_is_reported_before_script_start() {
        let error = match prepare_unix_python_command("print(1)".to_string(), None) {
            Err(error) => error,
            Ok(_) => panic!("missing Python must fail closed"),
        };

        assert!(error.contains("python_unavailable"));
    }

    #[test]
    fn python_probe_budget_reserves_time_for_later_candidates() {
        let started_at = Instant::now();
        let overall_deadline = started_at + Duration::from_secs(2);

        let first = python_probe_candidate_window(started_at, overall_deadline, 3).unwrap();
        assert!(first.execution_deadline < first.slot_deadline);
        assert_eq!(
            first.slot_deadline.duration_since(first.execution_deadline),
            PYTHON_PROBE_CLEANUP_BUDGET
        );
        let second =
            python_probe_candidate_window(first.slot_deadline, overall_deadline, 2).unwrap();
        assert!(second.execution_deadline < second.slot_deadline);
        let third =
            python_probe_candidate_window(second.slot_deadline, overall_deadline, 1).unwrap();
        assert_eq!(third.slot_deadline, overall_deadline);
        assert!(
            python_probe_candidate_window(
                started_at,
                started_at + PYTHON_PROBE_CLEANUP_BUDGET,
                1,
            )
            .is_none(),
            "an exact cleanup-only slot has no safe execution budget"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_python_candidate_cannot_starve_a_valid_fallback() {
        use std::os::unix::fs::PermissionsExt as _;

        let Ok(python3) = which::which("python3") else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let hanging = root.path().join("hanging-python");
        std::fs::write(&hanging, "#!/bin/sh\ntrap '' INT TERM\nsleep 30\n").unwrap();
        let mut permissions = std::fs::metadata(&hanging).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hanging, permissions).unwrap();

        let (tool, _) = tool(root.path().to_path_buf());
        let started_at = Instant::now();
        let (_, interpreter) = tool
            .resolve_python(
                "print('fallback')".to_owned(),
                vec![
                    PythonCandidate {
                        program: hanging,
                        prefix_args: Vec::new(),
                        display: "hanging-python".to_owned(),
                    },
                    PythonCandidate {
                        display: python3.display().to_string(),
                        program: python3,
                        prefix_args: Vec::new(),
                    },
                ],
                root.path(),
                &BTreeMap::new(),
                started_at,
                started_at + Duration::from_secs(5),
            )
            .await
            .expect("the valid second candidate should be selected");

        assert_ne!(interpreter, "hanging-python");
        assert!(started_at.elapsed() <= PYTHON_PROBE_MAX + Duration::from_millis(250));
    }

    #[tokio::test]
    async fn sub_cleanup_budget_is_a_script_timeout_not_python_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let (tool, _) = tool(root.path().to_path_buf());
        let started_at = Instant::now();
        let result = tool
            .resolve_python(
                "print('never-started')".to_owned(),
                vec![PythonCandidate {
                    program: PathBuf::from("candidate-does-not-need-to-exist"),
                    prefix_args: Vec::new(),
                    display: "candidate".to_owned(),
                }],
                root.path(),
                &BTreeMap::new(),
                started_at,
                started_at + Duration::from_millis(100),
            )
            .await;
        let Err(error) = result else {
            panic!("insufficient validation budget must fail before process start");
        };

        assert!(error.content.contains("script timeout"), "{}", error.content);
        assert!(!error.content.contains("python_unavailable"), "{}", error.content);
    }

    #[test]
    fn shell_script_uses_literal_shell_semantics() {
        let prepared = requested_invocation(&json!({
            "script": "param($Name)\nWrite-Output $Name",
            "language": "shell",
            "timeout": 1000
        }))
        .expect("shell script should prepare");

        let PreparedCommand::Ready(CommandSpec::Shell { shell, script }) = prepared.command else {
            panic!("shell source must remain a shell command");
        };
        assert_eq!(script, "param($Name)\nWrite-Output $Name");
        assert_eq!(
            shell,
            if cfg!(windows) {
                ShellKind::PowerShellLiteral
            } else {
                ShellKind::Posix
            }
        );
    }

    #[test]
    fn timeout_lost_outcome_preserves_the_pre_cleanup_snapshot() {
        let captured = OutputSnapshot {
            chunks: vec![nomi_execution::OutputChunk {
                seq: 1,
                start: 0,
                stream: OutputStream::Stdout,
                bytes: b"partial-marker\n".to_vec(),
                text: "partial-marker\n".to_string(),
            }],
            next_cursor: nomi_execution::OutputCursor::new(15),
            retained_bytes: 15,
            dropped_bytes: 0,
            encoding: nomi_execution::EncodingMetadata::default(),
        };
        let now = Instant::now();
        let outcome = ExecutionOutcome::Lost {
            last_known: nomi_execution::ProcessSnapshot {
                pid: 42,
                state: nomi_execution::ProcessState::Lost,
                started_at: now,
                last_activity_at: now,
            },
            output: OutputSnapshot::default(),
            cleanup: nomi_execution::CleanupReport::default(),
        };

        let result = render_script_timeout_cleanup(Ok(outcome), &captured);

        assert!(result.is_error);
        assert!(result.content.contains("partial-marker"));
        assert!(result.content.contains("process lost"));
    }

    #[test]
    fn lost_terminal_renderer_preserves_exact_cursor_gap_metadata() {
        let output = OutputSnapshot {
            chunks: Vec::new(),
            next_cursor: nomi_execution::OutputCursor::new(100),
            retained_bytes: 20,
            dropped_bytes: 80,
            encoding: nomi_execution::EncodingMetadata::default(),
        };
        let now = Instant::now();
        let result = render_terminal_with_missed(
            ExecutionOutcome::Lost {
                last_known: nomi_execution::ProcessSnapshot {
                    pid: 42,
                    state: nomi_execution::ProcessState::Lost,
                    started_at: now,
                    last_activity_at: now,
                },
                output,
                cleanup: nomi_execution::CleanupReport::default(),
            },
            Transport::Pipe,
            Some(37),
        );

        assert!(result.content.contains("missed_bytes=37"), "{}", result.content);
        assert!(result.content.contains("dropped_bytes=80"), "{}", result.content);
    }

    #[test]
    fn describe_script_mode_has_language_and_bounded_preview() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let description = tool.describe(&json!({
            "script": format!("preview-marker{}secret-tail", "x".repeat(200)),
            "language": "python",
            "timeout": 1000
        }));

        assert!(description.contains("python"));
        assert!(description.contains("preview-marker"));
        assert!(!description.contains("secret-tail"));
    }

    #[test]
    fn description_preserves_shell_and_tui_guidance() {
        let (tool, _) = tool(std::env::current_dir().unwrap());
        let description = tool.description();

        assert!(description.contains("Get-ChildItem"));
        assert!(description.contains("$env:NAME"));
        assert!(description.contains("cmd /C"));
        assert!(description.contains("\"\\r\") as its own write_stdin call"));
    }
}
