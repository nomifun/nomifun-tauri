use crate::error::DbError;
use crate::models::{TerminalSessionRow, TerminalTurnAdmissionRow};
use nomifun_common::{TerminalId, UserId};
use std::fmt;

/// Parameters for creating a terminal session row.
///
/// `id` is the caller-minted bare UUIDv7 business key stored as
/// `terminal_sessions.terminal_id`; SQLite allocates the row's integer `id`.
#[derive(Debug, Clone)]
pub struct CreateTerminalParams {
    pub id: TerminalId,
    pub name: String,
    pub cwd: String,
    pub command: String,
    /// JSON array of args.
    pub args: String,
    /// JSON object of env vars, nullable.
    pub env: Option<String>,
    pub backend: Option<String>,
    pub mode: Option<String>,
    pub cols: i64,
    pub rows: i64,
    pub user_id: UserId,
}

/// Stable business scope of one automatic turn admitted to a PTY.
///
/// The PTY epoch is the existing `PtyHandle` spawn generation. The durable
/// Requirement claim generation prevents a later retry from aliasing the prior
/// attempt. `turn_token` is added by the repository and must accompany every
/// later mutation.
#[derive(Clone, PartialEq, Eq)]
pub struct TerminalTurnAdmissionScope {
    pub terminal_id: String,
    pub pty_epoch: u64,
    pub requirement_id: String,
    pub claim_generation: i64,
    pub claim_token: String,
}

impl fmt::Debug for TerminalTurnAdmissionScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalTurnAdmissionScope")
            .field("terminal_id", &self.terminal_id)
            .field("pty_epoch", &self.pty_epoch)
            .field("requirement_id", &self.requirement_id)
            .field("claim_generation", &self.claim_generation)
            .field("claim_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TerminalTurnAdmissionKey {
    pub terminal_id: String,
    pub pty_epoch: u64,
    pub requirement_id: String,
    pub claim_generation: i64,
    pub claim_token: String,
    pub turn_token: String,
}

impl fmt::Debug for TerminalTurnAdmissionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalTurnAdmissionKey")
            .field("terminal_id", &self.terminal_id)
            .field("pty_epoch", &self.pty_epoch)
            .field("requirement_id", &self.requirement_id)
            .field("claim_generation", &self.claim_generation)
            .field("claim_token", &"<redacted>")
            .field("turn_token", &"<redacted>")
            .finish()
    }
}

impl TerminalTurnAdmissionKey {
    pub fn from_row(row: &TerminalTurnAdmissionRow) -> Result<Self, DbError> {
        let pty_epoch = u64::try_from(row.pty_epoch)
            .map_err(|_| DbError::Init("terminal turn receipt contains a negative PTY epoch".into()))?;
        let claim_token = row
            .claim_token
            .as_deref()
            .filter(|token| {
                token.len() == 64
                    && token
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(|| {
                DbError::Init(
                    "terminal turn receipt contains no valid Requirement claim capability".into(),
                )
            })?;
        Ok(Self {
            terminal_id: row.terminal_id.clone(),
            pty_epoch,
            requirement_id: row.requirement_id.clone(),
            claim_generation: row.claim_generation,
            claim_token: claim_token.to_owned(),
            turn_token: row.turn_token.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTurnAdmissionClaim {
    pub row: TerminalTurnAdmissionRow,
    /// True only for the atomic INSERT winner. Every existing row is absorbing;
    /// followers must never write PTY bytes.
    pub claimed_new: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalTurnEffectsStart {
    Started,
    AlreadyStarted,
    AlreadySettled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalTurnOutcome {
    Done,
    Failed,
    NeedsReview,
    Cancelled,
}

impl TerminalTurnOutcome {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::NeedsReview => "needs_review",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTurnSettlement {
    pub row: TerminalTurnAdmissionRow,
    /// True only when this call performed the immutable settlement.
    pub settled_new: bool,
}

/// Data access abstraction for the `terminal_sessions` table.
#[async_trait::async_trait]
pub trait ITerminalRepository: Send + Sync {
    /// Inserts a new terminal session row (status defaults to "running"). The id
    /// is allocated by SQLite and returned on the row.
    async fn create(&self, params: &CreateTerminalParams) -> Result<TerminalSessionRow, DbError>;

    /// Returns a single session by ID, or `None` if not found.
    async fn get_by_id(&self, id: &str) -> Result<Option<TerminalSessionRow>, DbError>;

    /// Returns all sessions for a user, newest first.
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<TerminalSessionRow>, DbError>;

    /// Returns every terminal session. Used only by coordinated application
    /// shutdown so `delete_all` can still dispatch each session's lifecycle
    /// hooks after the database transaction commits.
    async fn list_all(&self) -> Result<Vec<TerminalSessionRow>, DbError>;

    /// Updates the run status (and optional exit code) of a session.
    /// Returns `DbError::NotFound` if absent.
    async fn update_status(&self, id: &str, last_status: &str, exit_code: Option<i64>) -> Result<(), DbError>;

    /// Boot reconciliation: mark every `running` row as `exited` (exit_code
    /// NULL). At startup the in-memory live PTY map is empty, so any row still
    /// flagged `running` is a ghost from a prior process that died with the app
    /// —flipping it to `exited` makes the state honest (the frontend then shows
    /// the relaunch entry; a cron-bound terminal's fire-time `live` check sees
    /// `false` and relaunches instead of writing to a dead handle). Returns the
    /// number of rows reconciled.
    async fn mark_all_running_exited(&self) -> Result<u64, DbError>;

    /// Upsert the persisted scrollback (output history) snapshot for a session.
    /// Bounded to the in-memory cap (~256 KB) by the caller; written by the
    /// debounced flusher and on process exit —never per output chunk.
    async fn save_scrollback(&self, id: &str, data: &[u8]) -> Result<(), DbError>;

    /// Load the persisted scrollback for a session, or `None` if absent.
    /// Used by `get` to repopulate the reconnect snapshot when there is no live
    /// PTY handle (i.e. after an app restart).
    async fn load_scrollback(&self, id: &str) -> Result<Option<Vec<u8>>, DbError>;

    /// Drop the persisted scrollback for a session (idempotent —absent is OK).
    /// Called on relaunch so a fresh process does not show pre-relaunch history
    /// after a subsequent restart. Session deletion performs the same logical
    /// cleanup explicitly in the repository transaction.
    async fn clear_scrollback(&self, id: &str) -> Result<(), DbError>;

    /// Updates the stored terminal dimensions.
    async fn update_size(&self, id: &str, cols: i64, rows: i64) -> Result<(), DbError>;

    /// Updates name and/or pinned state. `name`/`pinned` of `None` are left
    /// unchanged; setting `pinned` also stamps/clears `pinned_at`.
    async fn update_meta(&self, id: &str, name: Option<&str>, pinned: Option<bool>) -> Result<(), DbError>;

    /// Rewrite the launch identity (command/args/backend) of a session in place.
    /// Used by the "fall back to a plain shell" path: the session keeps its id
    /// but its stored process becomes the login shell, so a later restart /
    /// boot-reconcile relaunches a shell (not the dead agent CLI) and the
    /// mechanical `default_name` becomes `Shell`. `args` is a JSON array string.
    /// Returns `DbError::NotFound` if absent.
    async fn update_command(
        &self,
        id: &str,
        command: &str,
        args: &str,
        backend: Option<&str>,
    ) -> Result<(), DbError>;

    /// Atomically rewrite launch identity and mark the row running immediately
    /// before an in-place shell replacement. Keeping these fields in one DB
    /// statement prevents a cancelled/failed preflight from exposing a shell
    /// identity with the old agent status (or vice versa).
    async fn update_launch_state(
        &self,
        id: &str,
        command: &str,
        args: &str,
        backend: Option<&str>,
        last_status: &str,
        exit_code: Option<i64>,
    ) -> Result<(), DbError>;

    /// Writes (or clears with `None`) the AutoWork config JSON blob for a session.
    /// Returns `DbError::NotFound` if absent.
    async fn update_autowork(&self, id: &str, autowork: Option<&str>) -> Result<(), DbError>;

    /// Writes (or clears with `None`) the IDMM config JSON blob for a session.
    /// Implementations validate and logically lock every per-watch
    /// `bypass_model.provider_id` in the same transaction as the write.
    /// Returns `DbError::NotFound` if absent.
    async fn update_idmm(&self, id: &str, idmm: Option<&str>) -> Result<(), DbError>;

    /// Reads the IDMM config JSON blob for a session.
    /// Returns `None` if the column is NULL or the session is not found.
    async fn get_idmm(&self, id: &str) -> Result<Option<String>, DbError>;

    /// Atomically admit one automatic turn for an exact PTY + Requirement claim.
    ///
    /// Only `claimed_new=true` owns the right to progress toward a PTY write.
    /// Existing `admitted`, `effects_started`, and `settled` rows all absorb
    /// replays, including restarts and lost responses.
    async fn claim_turn_admission(
        &self,
        _scope: &TerminalTurnAdmissionScope,
        _now: i64,
    ) -> Result<TerminalTurnAdmissionClaim, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement durable turn admission".into(),
        ))
    }

    /// Persist the irreversible-boundary fence immediately before writing PTY
    /// bytes. `AlreadyStarted` and `AlreadySettled` are absorbing and must not
    /// result in another write.
    async fn mark_turn_effects_started(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _now: i64,
    ) -> Result<TerminalTurnEffectsStart, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement durable effect admission".into(),
        ))
    }

    /// Persist the first half of a two-part TUI submission. Prompt body bytes
    /// are non-replayable, but they do not execute the command until the submit
    /// key is written.
    async fn mark_turn_body_written(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _now: i64,
    ) -> Result<TerminalTurnEffectsStart, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement durable body admission".into(),
        ))
    }

    /// Revalidate the exact Requirement claim and authorize the submit key for
    /// a previously `body_written` two-part TUI turn.
    async fn mark_turn_submit_started(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _now: i64,
    ) -> Result<TerminalTurnEffectsStart, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement durable submit admission".into(),
        ))
    }

    /// Settle a receipt from an authoritative durable Requirement verdict or an
    /// explicit stop/recovery decision. A raw lifecycle event is not authority:
    /// current CLI hooks carry the PTY epoch but no per-turn token.
    async fn settle_turn_admission(
        &self,
        _key: &TerminalTurnAdmissionKey,
        _outcome: TerminalTurnOutcome,
        _detail: Option<&str>,
        _now: i64,
    ) -> Result<TerminalTurnSettlement, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement durable turn settlement".into(),
        ))
    }

    async fn get_turn_admission(
        &self,
        _key: &TerminalTurnAdmissionKey,
    ) -> Result<Option<TerminalTurnAdmissionRow>, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement durable turn receipt lookup".into(),
        ))
    }

    /// Look up the unique permanent receipt for one exact typed Terminal +
    /// Requirement capability. If a row exists for the same Requirement
    /// generation but the Terminal or secret token differs, implementations
    /// must return `Conflict`, never `None`; cleanup may interpret `None` as
    /// proof that PTY admission never happened.
    async fn get_turn_admission_for_claim(
        &self,
        _terminal_id: &str,
        _requirement_id: &str,
        _claim_generation: i64,
        _claim_token: &str,
    ) -> Result<Option<TerminalTurnAdmissionRow>, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement claim receipt lookup".into(),
        ))
    }

    /// Permanently park every non-settled automatic turn for a Terminal (or one
    /// exact PTY epoch) before kill, relaunch, process-exit recovery, or delete.
    /// The matching active Requirement claim is moved to `needs_review` in the
    /// same transaction. Returns the number of receipts newly parked.
    async fn park_open_turn_admissions(
        &self,
        _terminal_id: &str,
        _pty_epoch: Option<u64>,
        _detail: &str,
        _now: i64,
    ) -> Result<u64, DbError> {
        Err(DbError::Init(
            "terminal repository does not implement automatic-turn parking".into(),
        ))
    }

    /// Deletes a session row plus repository-owned logical dependents.
    ///
    /// `terminal_scrollback` and terminal-scoped knowledge bindings (including
    /// their junction rows) are removed in the same database transaction.
    /// Returns `DbError::NotFound` if absent.
    async fn delete(&self, id: &str) -> Result<(), DbError>;

    /// Deletes EVERY terminal session row (whole table) and explicitly removes
    /// all logical `terminal_scrollback` and terminal knowledge-binding
    /// dependents. Returns the number of rows deleted. Used only on real app
    /// exit (desktop quit) to wipe the dirty sessions a crashed/closed run
    /// would otherwise leave behind —never on close-to-tray. A clean exit with
    /// zero rows is normal and must NOT error.
    async fn delete_all(&self) -> Result<u64, DbError>;
}
