//! The capability the persistent AutoWork runner (in `nomifun-requirement`) uses to
//! drive a terminal's PTY as an execution substrate — write input, observe the
//! live output stream, check liveness, and read/write the terminal's AutoWork
//! config — without depending on this crate's internals. `TerminalService`
//! implements it (see `service.rs`).

use async_trait::async_trait;
use nomifun_db::{
    TerminalTurnAdmissionClaim, TerminalTurnAdmissionKey, TerminalTurnAdmissionRow,
    TerminalTurnAdmissionScope, TerminalTurnEffectsStart, TerminalTurnOutcome,
    TerminalTurnSettlement,
};
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
    async fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), TerminalError>;

    /// Return the live PTY spawn generation, or `None` when no handle exists.
    fn current_epoch(&self, _id: &str) -> Option<u64> {
        None
    }

    /// Write only if the terminal still owns the exact PTY generation.
    ///
    /// The default fails closed so test doubles and alternative drivers cannot
    /// accidentally degrade an automatic turn to an unfenced write.
    async fn write_input_exact_epoch(
        &self,
        id: &str,
        _pty_epoch: u64,
        _bytes: &[u8],
    ) -> Result<(), TerminalError> {
        Err(TerminalError::StaleGeneration(format!(
            "terminal {id} does not support exact PTY generation writes"
        )))
    }

    /// Subscribe to a copy of the PTY's live output byte-stream. `None` if the
    /// session is not live.
    fn subscribe_output(&self, id: &str) -> Option<broadcast::Receiver<Vec<u8>>>;

    /// Whether the PTY is currently live (the child process is running here).
    fn is_alive(&self, id: &str) -> bool;

    /// Lightweight metadata for gating + ownership. `Ok(None)` if the row is gone.
    async fn describe(&self, id: &str) -> Result<Option<TerminalDescription>, TerminalError>;

    /// Read the raw AutoWork config JSON blob for a terminal (`None` if unset).
    async fn read_autowork(&self, id: &str) -> Result<Option<String>, TerminalError>;

    /// Write (or clear with `None`) the AutoWork config JSON blob for a terminal.
    async fn write_autowork(&self, id: &str, autowork: Option<&str>) -> Result<(), TerminalError>;

    /// Read the raw IDMM config JSON blob for a terminal (`None` if unset).
    async fn read_idmm(&self, id: &str) -> Result<Option<String>, TerminalError>;

    /// Write (or clear with `None`) the IDMM config JSON blob for a terminal.
    async fn write_idmm(&self, id: &str, idmm: Option<&str>) -> Result<(), TerminalError>;

    /// Subscribe to this terminal's structured lifecycle events (turn-end / tool /
    /// notification) from the in-process lifecycle server. `None` if lifecycle is
    /// not wired or the session is unknown. Used by AutoWork to await turn-end
    /// (Stop) instead of scraping the byte stream.
    fn subscribe_lifecycle(
        &self,
        id: &str,
    ) -> Option<broadcast::Receiver<crate::lifecycle::TerminalLifecycleEvent>>;

    /// Subscribe to lifecycle activity for exactly one PTY generation.
    /// Legacy/unscoped and delayed prior-generation events are rejected.
    fn subscribe_lifecycle_exact(
        &self,
        _id: &str,
        _pty_epoch: u64,
    ) -> Option<crate::lifecycle::ExactTerminalLifecycleReceiver> {
        None
    }

    /// Atomically mint or replay the durable right to execute an automatic PTY
    /// turn. Only `claimed_new=true` may advance toward effects.
    async fn claim_turn_admission(
        &self,
        _scope: &TerminalTurnAdmissionScope,
    ) -> Result<TerminalTurnAdmissionClaim, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement durable turn admission".into(),
        )))
    }

    /// Persist the irreversible-effects boundary. Only `Started` may write.
    async fn mark_turn_effects_started(
        &self,
        _key: &TerminalTurnAdmissionKey,
    ) -> Result<TerminalTurnEffectsStart, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement durable effect admission".into(),
        )))
    }

    /// Atomically fence the durable effects transition against PTY teardown,
    /// then write only for the unique `admitted -> effects_started` winner.
    ///
    /// Callers should use this instead of composing
    /// `mark_turn_effects_started` with `write_input_exact_epoch`.
    async fn write_admitted_turn(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _bytes: &[u8],
    ) -> Result<TerminalTurnEffectsStart, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement admitted turn writes".into(),
        )))
    }

    /// Write only the prompt body for a two-part TUI submission and persist the
    /// absorbing `body_written` phase. This does not authorize the submit key.
    async fn write_admitted_body(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _bytes: &[u8],
    ) -> Result<TerminalTurnEffectsStart, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement admitted body writes".into(),
        )))
    }

    /// Revalidate the exact receipt + Requirement generation, then write the
    /// submit key for the unique `body_written -> effects_started` winner.
    async fn write_admitted_submit(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _bytes: &[u8],
    ) -> Result<TerminalTurnEffectsStart, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement admitted submit writes".into(),
        )))
    }

    /// Absorb the turn under an authoritative durable outcome.
    async fn settle_turn_admission(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _outcome: TerminalTurnOutcome,
        _detail: Option<&str>,
    ) -> Result<TerminalTurnSettlement, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement durable turn settlement".into(),
        )))
    }

    async fn get_turn_admission(
        &self,
        _key: &TerminalTurnAdmissionKey,
    ) -> Result<Option<TerminalTurnAdmissionRow>, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement durable turn receipt lookup".into(),
        )))
    }

    /// Exact cleanup lookup. A receipt for the same generation under another
    /// Terminal or capability is an authority conflict, not absence.
    async fn get_turn_admission_for_claim(
        &self,
        _terminal_id: &str,
        _requirement_id: &str,
        _claim_generation: i64,
        _claim_token: &str,
    ) -> Result<Option<TerminalTurnAdmissionRow>, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement claim receipt lookup".into(),
        )))
    }

    /// Park ambiguous open turns before PTY teardown or recovery.
    async fn park_open_turn_admissions(
        &self,
        _id: &str,
        _pty_epoch: Option<u64>,
        _detail: &str,
    ) -> Result<u64, TerminalError> {
        Err(TerminalError::Database(nomifun_db::DbError::Init(
            "terminal driver does not implement automatic-turn parking".into(),
        )))
    }
}
