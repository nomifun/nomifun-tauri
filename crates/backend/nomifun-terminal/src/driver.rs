//! The capability the AutoWork orchestrator (in `nomifun-requirement`) uses to
//! drive a terminal's PTY as an execution substrate — write input, observe the
//! live output stream, check liveness, and read/write the terminal's AutoWork
//! config — without depending on this crate's internals. `TerminalService`
//! implements it (see `service.rs`).

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::error::TerminalError;

/// Lightweight terminal session metadata for AutoWork gating + ownership checks.
#[derive(Debug, Clone)]
pub struct TerminalDescription {
    pub user_id: String,
    /// Working directory the PTY was launched in. Consumers probe it for the
    /// `.nomi/knowledge/README.md` contract file to prepend knowledge guidance.
    pub cwd: String,
    /// The stored launch program (the `command` column; `$SHELL` sentinel for a
    /// plain shell). With `args` + `backend`, lets the AutoWork gate resolve the
    /// agent family the SAME way launch injection does (`terminal_autowork_capable`).
    pub command: String,
    /// The stored launch argv (the parsed `args` column). Carries the wrapped CLI
    /// token for wrapper launches (`stepcode claude` → `["claude", …]`).
    pub args: Vec<String>,
    /// Preset backend: "claude" | "codex" | "gemini" | None (plain shell / custom
    /// command). Only set when a preset declared it — do NOT use it alone for
    /// eligibility; resolve the family from `command`/`args`/`backend` together.
    pub backend: Option<String>,
    /// Permission mode label: "default" | "full-auto" | None.
    pub mode: Option<String>,
    /// "running" | "exited" | "error".
    pub last_status: String,
}

#[async_trait]
pub trait TerminalDriver: Send + Sync {
    /// Write raw bytes to the PTY stdin. `Err(NotFound)` if the session is not live.
    async fn write_input(&self, id: i64, bytes: &[u8]) -> Result<(), TerminalError>;

    /// Subscribe to a copy of the PTY's live output byte-stream. `None` if the
    /// session is not live.
    fn subscribe_output(&self, id: i64) -> Option<broadcast::Receiver<Vec<u8>>>;

    /// Whether the PTY is currently live (the child process is running here).
    fn is_alive(&self, id: i64) -> bool;

    /// Lightweight metadata for gating + ownership. `Ok(None)` if the row is gone.
    async fn describe(&self, id: i64) -> Result<Option<TerminalDescription>, TerminalError>;

    /// Read the raw AutoWork config JSON blob for a terminal (`None` if unset).
    async fn read_autowork(&self, id: i64) -> Result<Option<String>, TerminalError>;

    /// Write (or clear with `None`) the AutoWork config JSON blob for a terminal.
    async fn write_autowork(&self, id: i64, autowork: Option<&str>) -> Result<(), TerminalError>;

    /// Read the raw IDMM config JSON blob for a terminal (`None` if unset).
    async fn read_idmm(&self, id: i64) -> Result<Option<String>, TerminalError>;

    /// Write (or clear with `None`) the IDMM config JSON blob for a terminal.
    async fn write_idmm(&self, id: i64, idmm: Option<&str>) -> Result<(), TerminalError>;

    /// Subscribe to this terminal's structured lifecycle events (turn-end / tool /
    /// notification) from the in-process lifecycle server. `None` if lifecycle is
    /// not wired or the session is unknown. Used by AutoWork to await turn-end
    /// (Stop) instead of scraping the byte stream.
    fn subscribe_lifecycle(
        &self,
        id: i64,
    ) -> Option<broadcast::Receiver<crate::lifecycle::TerminalLifecycleEvent>>;
}
