use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use nomi_process_runtime::{
    CapabilityPolicy, CommandSpec, OutputCursor, OutputSnapshot, OutputStream, PollResult,
    ProcessOutcome, ProcessOwner, ProcessPolicy, ProcessRequest, ProcessSupervisor, ShellKind,
    Transport, normalize_request,
};
use tokio::process::Command;
use uuid::Uuid;

const POWERSHELL_EXE: &str = "powershell.exe";
const POWERSHELL_ARGS: &[&str] = &[
    "-NoLogo",
    "-NoProfile",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];
const SH_ARGS: &[&str] = &["-c"];

pub struct ShellInfo {
    pub program: &'static str,
    pub args_before_command: &'static [&'static str],
    pub syntax_name: &'static str,
}

#[derive(Clone)]
pub struct SupervisedShell {
    supervisor: Arc<ProcessSupervisor>,
    capability: CapabilityPolicy,
    invocation_id: Uuid,
}

#[derive(Debug)]
pub struct SupervisedShellOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisedShellError {
    #[error("shell process failed: {0}")]
    Process(#[from] nomi_process_runtime::ProcessError),
    #[error("shell process timed out after {timeout_ms}ms")]
    TimedOut { timeout_ms: u64 },
    #[error("shell process was cancelled before completion")]
    Cancelled,
    #[error("shell process ownership was lost: {0}")]
    Lost(String),
    #[error("shell process failed to spawn: {0}")]
    SpawnFailed(String),
}

impl SupervisedShell {
    pub fn new(supervisor: Arc<ProcessSupervisor>, cwd_root: PathBuf) -> Self {
        Self {
            supervisor,
            capability: CapabilityPolicy::local_owner(cwd_root),
            invocation_id: Uuid::now_v7(),
        }
    }

    pub fn standalone(cwd_root: PathBuf) -> Self {
        Self::new(
            ProcessSupervisor::new(nomi_process_runtime::SupervisorConfig::default()),
            cwd_root,
        )
    }

    pub fn supervisor(&self) -> &Arc<ProcessSupervisor> {
        &self.supervisor
    }

    /// Execute one shell command under the shared exact process-tree
    /// supervisor. A returned success/error is emitted only after the complete
    /// process tree has been reaped. If the caller future is cancelled, the
    /// registered session remains owned by the supervisor and is drained by
    /// the turn's `ProcessSupervisor::quiesce` fence.
    pub async fn output(
        &self,
        command: &str,
        cwd: &Path,
        env: &HashMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<SupervisedShellOutput, SupervisedShellError> {
        let started_at = Instant::now();
        let deadline = timeout.and_then(|duration| started_at.checked_add(duration));
        let request = ProcessRequest {
            owner: ProcessOwner::new(self.invocation_id, Uuid::now_v7()),
            command: CommandSpec::Shell {
                shell: if cfg!(windows) {
                    ShellKind::PowerShell
                } else {
                    ShellKind::Posix
                },
                script: command.to_owned(),
            },
            cwd: cwd.to_path_buf(),
            env: env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect::<BTreeMap<_, _>>(),
            transport: Transport::Pipe,
            policy: ProcessPolicy {
                deadline,
                ..ProcessPolicy::default()
            },
            capability: self.capability.clone(),
        };
        let request = normalize_request(request, cwd)?;
        let handle = self.supervisor.start(request).await?;

        let outcome = loop {
            let yield_until = deadline.unwrap_or_else(|| {
                Instant::now()
                    .checked_add(Duration::from_secs(60 * 60))
                    .unwrap_or_else(Instant::now)
            });
            match self
                .supervisor
                .poll(
                    &handle.owner,
                    &handle.session_id,
                    OutputCursor::START,
                    yield_until,
                )
                .await
            {
                Ok(PollResult::Finished(outcome)) => break outcome,
                Ok(PollResult::Running { .. })
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) =>
                {
                    break self
                        .supervisor
                        .timeout(&handle.owner, &handle.session_id)
                        .await?;
                }
                Ok(PollResult::Running { .. }) => {}
                Err(error) => {
                    let _ = self
                        .supervisor
                        .cancel(&handle.owner, &handle.session_id)
                        .await;
                    return Err(error.into());
                }
            }
        };

        match outcome {
            ProcessOutcome::Exited {
                code,
                signal,
                output,
                ..
            } => Ok(SupervisedShellOutput {
                success: code == Some(0) && signal.is_none(),
                code,
                stdout: output_stream_text(&output, OutputStream::Stdout),
                stderr: output_stream_text(&output, OutputStream::Stderr),
            }),
            ProcessOutcome::TimedOut { .. } => Err(SupervisedShellError::TimedOut {
                timeout_ms: timeout
                    .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or_default(),
            }),
            ProcessOutcome::Cancelled { .. } => Err(SupervisedShellError::Cancelled),
            ProcessOutcome::Lost { cleanup, .. } => {
                Err(SupervisedShellError::Lost(cleanup.errors.join("; ")))
            }
            ProcessOutcome::SpawnFailed(failure) => Err(SupervisedShellError::SpawnFailed(
                format!("{}: {}", failure.code, failure.message),
            )),
        }
    }
}

fn output_stream_text(output: &OutputSnapshot, stream: OutputStream) -> String {
    output
        .chunks
        .iter()
        .filter(|chunk| chunk.stream == stream)
        .map(|chunk| chunk.text.as_str())
        .collect()
}

pub fn shell_info() -> ShellInfo {
    if cfg!(windows) {
        ShellInfo {
            program: POWERSHELL_EXE,
            args_before_command: POWERSHELL_ARGS,
            syntax_name: "PowerShell",
        }
    } else {
        ShellInfo {
            program: "sh",
            args_before_command: SH_ARGS,
            syntax_name: "POSIX sh",
        }
    }
}

pub fn shell_command_args(command_str: &str) -> Vec<String> {
    let info = shell_info();
    let mut args = info
        .args_before_command
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect::<Vec<_>>();
    args.push(shell_command_payload(command_str));
    args
}

pub fn shell_command_builder(command_str: &str) -> Command {
    let info = shell_info();
    let mut cmd = Command::new(info.program);
    cmd.args(shell_command_args(command_str));
    // CREATE_NO_WINDOW: don't flash a console window when the host is a GUI app.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

fn shell_command_payload(command_str: &str) -> String {
    if cfg!(windows) {
        powershell_payload(command_str)
    } else {
        command_str.to_owned()
    }
}

#[cfg(windows)]
fn powershell_payload(command_str: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $global:LASTEXITCODE = $null\n\
         try {{\n\
         & {{\n\
         {command_str}\n\
         }}\n\
         if ($null -ne $global:LASTEXITCODE) {{ exit $global:LASTEXITCODE }}\n\
         if (-not $?) {{ exit 1 }}\n\
         exit 0\n\
         }} catch {{\n\
         [Console]::Error.WriteLine($_.Exception.Message)\n\
         exit 1\n\
         }}"
    )
}

#[cfg(not(windows))]
fn powershell_payload(command_str: &str) -> String {
    command_str.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_info_returns_platform_appropriate_values() {
        let info = shell_info();
        if cfg!(windows) {
            assert_eq!(info.program, "powershell.exe");
            assert_eq!(info.args_before_command, POWERSHELL_ARGS);
            assert_eq!(info.syntax_name, "PowerShell");
        } else {
            assert_eq!(info.program, "sh");
            assert_eq!(info.args_before_command, SH_ARGS);
            assert_eq!(info.syntax_name, "POSIX sh");
        }
    }

    #[tokio::test]
    async fn shell_command_builder_allows_env_and_cwd() {
        let tmp = std::env::temp_dir();
        let cmd_str = if cfg!(windows) {
            "Write-Output $env:MY_VAR"
        } else {
            "echo $MY_VAR"
        };
        let output = shell_command_builder(cmd_str)
            .env("MY_VAR", "test_value")
            .current_dir(&tmp)
            .output()
            .await
            .expect("builder failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("test_value"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_command_builder_accepts_powershell_syntax_on_windows() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("proof.txt"), "ok").unwrap();
        let output = shell_command_builder(
            "if (Test-Path proof.txt) { Get-Content proof.txt } else { exit 9 }",
        )
        .current_dir(tmp.path())
        .output()
        .await
        .expect("builder failed");

        assert!(
            output.status.success(),
            "status: {:?}",
            output.status.code()
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("ok"), "stdout: {stdout}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_command_builder_preserves_native_exit_code_on_windows() {
        let output = shell_command_builder("cmd /c exit 7")
            .output()
            .await
            .expect("builder failed");

        assert_eq!(output.status.code(), Some(7));
    }
}
