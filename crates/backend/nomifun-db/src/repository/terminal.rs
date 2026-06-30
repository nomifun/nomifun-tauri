use crate::error::DbError;
use crate::models::TerminalSessionRow;

/// Parameters for creating a terminal session row.
///
/// `id` is allocated by SQLite (INTEGER PK AUTOINCREMENT) on create and returned
/// on the resulting row; it is not supplied by the caller.
#[derive(Debug, Clone)]
pub struct CreateTerminalParams {
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
    pub user_id: String,
}

/// Data access abstraction for the `terminal_sessions` table.
#[async_trait::async_trait]
pub trait ITerminalRepository: Send + Sync {
    /// Inserts a new terminal session row (status defaults to "running"). The id
    /// is allocated by SQLite and returned on the row.
    async fn create(&self, params: &CreateTerminalParams) -> Result<TerminalSessionRow, DbError>;

    /// Returns a single session by ID, or `None` if not found.
    async fn get_by_id(&self, id: i64) -> Result<Option<TerminalSessionRow>, DbError>;

    /// Returns all sessions for a user, newest first.
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<TerminalSessionRow>, DbError>;

    /// Updates the run status (and optional exit code) of a session.
    /// Returns `DbError::NotFound` if absent.
    async fn update_status(&self, id: i64, last_status: &str, exit_code: Option<i64>) -> Result<(), DbError>;

    /// Boot reconciliation: mark every `running` row as `exited` (exit_code
    /// NULL). At startup the in-memory live PTY map is empty, so any row still
    /// flagged `running` is a ghost from a prior process that died with the app
    /// — flipping it to `exited` makes the state honest (the frontend then shows
    /// the relaunch entry; a cron-bound terminal's fire-time `live` check sees
    /// `false` and relaunches instead of writing to a dead handle). Returns the
    /// number of rows reconciled.
    async fn mark_all_running_exited(&self) -> Result<u64, DbError>;

    /// Upsert the persisted scrollback (output history) snapshot for a session.
    /// Bounded to the in-memory cap (~256 KB) by the caller; written by the
    /// debounced flusher and on process exit — never per output chunk.
    async fn save_scrollback(&self, id: i64, data: &[u8]) -> Result<(), DbError>;

    /// Load the persisted scrollback for a session, or `None` if absent.
    /// Used by `get` to repopulate the reconnect snapshot when there is no live
    /// PTY handle (i.e. after an app restart).
    async fn load_scrollback(&self, id: i64) -> Result<Option<Vec<u8>>, DbError>;

    /// Drop the persisted scrollback for a session (idempotent — absent is OK).
    /// Called on relaunch so a fresh process does not show pre-relaunch history
    /// after a subsequent restart. (Session deletion is handled by the FK
    /// `ON DELETE CASCADE`, so `delete` needs no extra call.)
    async fn clear_scrollback(&self, id: i64) -> Result<(), DbError>;

    /// Updates the stored terminal dimensions.
    async fn update_size(&self, id: i64, cols: i64, rows: i64) -> Result<(), DbError>;

    /// Updates name and/or pinned state. `name`/`pinned` of `None` are left
    /// unchanged; setting `pinned` also stamps/clears `pinned_at`.
    async fn update_meta(&self, id: i64, name: Option<&str>, pinned: Option<bool>) -> Result<(), DbError>;

    /// Rewrite the launch identity (command/args/backend) of a session in place.
    /// Used by the "fall back to a plain shell" path: the session keeps its id
    /// but its stored process becomes the login shell, so a later restart /
    /// boot-reconcile relaunches a shell (not the dead agent CLI) and the
    /// mechanical `default_name` becomes `Shell`. `args` is a JSON array string.
    /// Returns `DbError::NotFound` if absent.
    async fn update_command(
        &self,
        id: i64,
        command: &str,
        args: &str,
        backend: Option<&str>,
    ) -> Result<(), DbError>;

    /// Writes (or clears with `None`) the AutoWork config JSON blob for a session.
    /// Returns `DbError::NotFound` if absent.
    async fn update_autowork(&self, id: i64, autowork: Option<&str>) -> Result<(), DbError>;

    /// Writes (or clears with `None`) the IDMM config JSON blob for a session.
    /// Returns `DbError::NotFound` if absent.
    async fn update_idmm(&self, id: i64, idmm: Option<&str>) -> Result<(), DbError>;

    /// Reads the IDMM config JSON blob for a session.
    /// Returns `None` if the column is NULL or the session is not found.
    async fn get_idmm(&self, id: i64) -> Result<Option<String>, DbError>;

    /// Deletes a session row. Returns `DbError::NotFound` if absent.
    async fn delete(&self, id: i64) -> Result<(), DbError>;

    /// Deletes EVERY terminal session row (whole table). The
    /// `terminal_scrollback` rows are dropped by the FK `ON DELETE CASCADE`, so
    /// no second call is needed. Returns the number of rows deleted. Used only on
    /// real app exit (desktop quit) to wipe the dirty sessions a crashed/closed
    /// run would otherwise leave behind — never on close-to-tray. A clean exit
    /// with zero rows is normal and must NOT error.
    async fn delete_all(&self) -> Result<u64, DbError>;
}
