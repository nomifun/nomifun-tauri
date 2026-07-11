use std::{io, sync::Arc, time::Instant};

use async_trait::async_trait;

use crate::{ExecutionError, NormalizedExecutionRequest, OutputBuffer, Transport};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
mod unix_pty;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod linux_watchdog;
#[cfg(target_os = "macos")]
mod macos_watchdog;
#[cfg(unix)]
mod unix_protocol;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExitFact {
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) cleanup_errors: Vec<String>,
}

#[async_trait]
pub(crate) trait ProcessOwner: Send + Sync {
    fn pid(&self) -> u32;
    async fn write(&self, bytes: &[u8]) -> io::Result<()>;
    async fn close_stdin(&self) -> io::Result<()>;
    async fn resize(&self, _cols: u16, _rows: u16) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process transport does not support terminal resize",
        ))
    }
    async fn interrupt(&self) -> io::Result<()>;
    async fn terminate(&self) -> io::Result<()>;
    async fn force_kill(&self) -> io::Result<()>;
    async fn wait_reaped(&self, deadline: Instant) -> io::Result<ExitFact>;
}

pub(crate) struct SpawnedPlatformProcess {
    pub(crate) owner: Arc<dyn ProcessOwner>,
}

pub(crate) async fn spawn(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    match request.transport {
        Transport::Pipe => spawn_pipe(request, output).await,
        Transport::Pty { cols, rows } => spawn_pty(request, output, cols, rows).await,
    }
}

pub(crate) async fn spawn_pipe(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    #[cfg(unix)]
    {
        unix::spawn_pipe(request, output).await
    }

    #[cfg(windows)]
    {
        windows::spawn_pipe(request, output).await
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (request, output);
        Err(ExecutionError::Transport {
            reason: "platform pipe adapter is pending".to_owned(),
        })
    }
}

pub(crate) async fn spawn_pty(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    cols: u16,
    rows: u16,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    #[cfg(unix)]
    {
        unix::spawn_pty(request, output, cols, rows).await
    }

    #[cfg(windows)]
    {
        windows::spawn_pty(request, output, cols, rows).await
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (request, output, cols, rows);
        Err(ExecutionError::Transport {
            reason: "platform PTY adapter is unavailable".to_owned(),
        })
    }
}
