use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    CapabilityPolicy,
    outcome::{CleanupReport, ProcessSnapshot, SessionId, SpawnFailure},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRequest {
    pub owner: ExecutionOwner,
    pub command: CommandSpec,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub transport: Transport,
    pub policy: ExecutionPolicy,
    pub capability: CapabilityPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandSpec {
    Program {
        program: OsString,
        args: Vec<OsString>,
    },
    Shell {
        shell: ShellKind,
        script: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Pipe,
    Pty { cols: u16, rows: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    /// PowerShell source passed literally after `-Command`, without the legacy
    /// compatibility prologue/exit-status wrapper.
    PowerShellLiteral,
    Posix,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExecutionOwner {
    pub run_id: Uuid,
    pub call_id: Uuid,
}

impl ExecutionOwner {
    pub const fn new(run_id: Uuid, call_id: Uuid) -> Self {
        Self { run_id, call_id }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub output_limit_bytes: usize,
    pub lease: Duration,
    pub deadline: Option<Instant>,
    pub interrupt_grace: Duration,
    pub terminate_grace: Duration,
    pub reap_grace: Duration,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            output_limit_bytes: 4 * 1024 * 1024,
            lease: Duration::from_secs(15 * 60),
            deadline: None,
            interrupt_grace: Duration::from_secs(1),
            terminate_grace: Duration::from_secs(1),
            reap_grace: Duration::from_secs(3),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedExecutionRequest {
    pub owner: ExecutionOwner,
    pub command: CommandSpec,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub transport: Transport,
    pub policy: ExecutionPolicy,
    pub capability: CapabilityPolicy,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("invalid working directory {path:?}: {reason}")]
    InvalidWorkingDirectory { path: PathBuf, reason: String },
    #[error("execution capability denied for {path:?}: {reason}")]
    CapabilityDenied { path: PathBuf, reason: String },
    #[error("invalid command: {reason}")]
    InvalidCommand { reason: String },
    #[error("invalid transport: {reason}")]
    InvalidTransport { reason: String },
    #[error("execution session {session_id} was not found")]
    SessionNotFound { session_id: SessionId },
    #[error("execution session {session_id} belongs to a different owner")]
    OwnerMismatch { session_id: SessionId },
    #[error("execution supervisor capacity is exhausted (max sessions: {max_sessions})")]
    CapacityExhausted { max_sessions: usize },
    #[error("execution supervisor is shutting down")]
    SupervisorShuttingDown,
    #[error("execution transport failure: {reason}")]
    Transport { reason: String },
    #[error("execution I/O failure during {operation}: {reason}")]
    Io {
        operation: &'static str,
        reason: String,
    },
    #[error("execution spawn failed: {failure:?}")]
    SpawnFailed { failure: SpawnFailure },
    #[error(
        "execution start ownership was lost: failure={failure:?}, last_known={last_known:?}, cleanup={cleanup:?}"
    )]
    StartLost {
        failure: SpawnFailure,
        last_known: Option<ProcessSnapshot>,
        cleanup: CleanupReport,
    },
}

impl ExecutionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidWorkingDirectory { .. } => "invalid_working_directory",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::InvalidCommand { .. } => "invalid_command",
            Self::InvalidTransport { .. } => "invalid_transport",
            Self::SessionNotFound { .. } => "session_not_found",
            Self::OwnerMismatch { .. } => "owner_mismatch",
            Self::CapacityExhausted { .. } => "capacity_exhausted",
            Self::SupervisorShuttingDown => "supervisor_shutting_down",
            Self::Transport { .. } => "transport",
            Self::Io { .. } => "io",
            Self::SpawnFailed { .. } => "spawn_failed",
            Self::StartLost { .. } => "start_lost",
        }
    }
}

pub fn normalize_request(
    request: ExecutionRequest,
    session_cwd: &Path,
) -> Result<NormalizedExecutionRequest, ExecutionError> {
    let requested_cwd = if request.cwd.is_absolute() {
        request.cwd.clone()
    } else {
        session_cwd.join(&request.cwd)
    };
    let cwd = canonicalize_compatible(&requested_cwd).map_err(|error| {
        ExecutionError::InvalidWorkingDirectory {
            path: requested_cwd.clone(),
            reason: error.to_string(),
        }
    })?;

    if !cwd.is_dir() {
        return Err(ExecutionError::InvalidWorkingDirectory {
            path: requested_cwd,
            reason: "path is not a directory".to_owned(),
        });
    }

    let mut canonical_roots = Vec::with_capacity(request.capability.cwd_roots.len());
    for root in &request.capability.cwd_roots {
        let canonical_root = canonicalize_compatible(root).map_err(|error| {
            ExecutionError::CapabilityDenied {
                path: cwd.clone(),
                reason: format!("could not resolve capability root {root:?}: {error}"),
            }
        })?;
        canonical_roots.push(canonical_root);
    }
    if !canonical_roots.iter().any(|root| cwd.starts_with(root)) {
        return Err(ExecutionError::CapabilityDenied {
            path: cwd,
            reason: "working directory is outside the allowed roots".to_owned(),
        });
    }

    if matches!(
        request.transport,
        Transport::Pty {
            cols: 0,
            ..
        } | Transport::Pty {
            rows: 0,
            ..
        }
    ) {
        return Err(ExecutionError::InvalidTransport {
            reason: "PTY dimensions must be non-zero".to_owned(),
        });
    }

    match &request.command {
        CommandSpec::Program { program, .. } if program.is_empty() => {
            return Err(ExecutionError::InvalidCommand {
                reason: "program must not be empty".to_owned(),
            });
        }
        CommandSpec::Shell { script, .. } if script.is_empty() => {
            return Err(ExecutionError::InvalidCommand {
                reason: "shell script must not be empty".to_owned(),
            });
        }
        CommandSpec::Program { .. } | CommandSpec::Shell { .. } => {}
    }

    let mut capability = request.capability;
    capability.cwd_roots = canonical_roots;

    Ok(NormalizedExecutionRequest {
        owner: request.owner,
        command: request.command,
        cwd,
        env: request.env,
        transport: request.transport,
        policy: request.policy,
        capability,
    })
}

fn canonicalize_compatible(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;

    #[cfg(windows)]
    if let Some(simplified) = strip_verbatim_disk_prefix(&canonical)
        && matches!(fs::canonicalize(&simplified), Ok(round_trip) if round_trip == canonical)
    {
        return Ok(simplified);
    }

    Ok(canonical)
}

#[cfg(windows)]
fn strip_verbatim_disk_prefix(path: &Path) -> Option<PathBuf> {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Component, Prefix},
    };

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    if !matches!(prefix.kind(), Prefix::VerbatimDisk(_)) {
        return None;
    }

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    const VERBATIM_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    encoded
        .strip_prefix(&VERBATIM_PREFIX)
        .map(OsString::from_wide)
        .map(PathBuf::from)
}
