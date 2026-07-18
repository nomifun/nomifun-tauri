use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use fs2::FileExt;
use sqlx::migrate::Migrator;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection, Row, Sqlite, SqlitePool};
use tracing::{info, warn};

use crate::error::DbError;

/// Maximum number of connections in the pool.
const MAX_CONNECTIONS: u32 = 5;

/// SQLite busy timeout in milliseconds.
const BUSY_TIMEOUT_MS: u64 = 5000;
const STARTUP_FILE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

static DB_MIGRATOR: Migrator = sqlx::migrate!();

/// Wraps a SQLite connection pool with lifecycle management.
#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Returns a reference to the underlying connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Closes all connections in the pool.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Create a transactionally consistent SQLite snapshot at `destination`.
    ///
    /// This reads through SQLite rather than copying the main file, so
    /// committed pages still resident in WAL are included. The caller is
    /// responsible for placing the snapshot in a broader bundle manifest with
    /// the dataset generation and checksums for non-database files.
    pub async fn snapshot_into(&self, destination: &Path) -> Result<(), DbError> {
        if destination.exists() {
            return Err(DbError::Conflict(format!(
                "snapshot destination already exists: {}",
                destination.display()
            )));
        }
        if let Some(parent) = destination.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                DbError::Init(format!(
                    "failed to create snapshot directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let destination_text = destination.to_str().ok_or_else(|| {
            DbError::SafetyBackup(format!(
                "snapshot destination is not valid UTF-8: {}",
                destination.display()
            ))
        })?;
        sqlx::query("VACUUM main INTO ?")
            .bind(destination_text)
            .execute(&self.pool)
            .await
            .map_err(|error| {
                DbError::SafetyBackup(format!(
                    "could not create WAL-safe SQLite snapshot {}: {error}",
                    destination.display()
                ))
            })?;
        validate_sqlite_snapshot(destination).await
    }
}

pub(crate) async fn validate_sqlite_snapshot(path: &Path) -> Result<(), DbError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));
    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(DbError::Query)?;
    let result = async {
        validate_quick_check(&pool).await?;
        validate_restorable_database_contract(&pool).await
    }
    .await;
    pool.close().await;
    result
}

/// Open an existing ID-v2 database for an offline snapshot without running
/// migrations, recovery, or quarantine/rebuild logic against the source.
///
/// Backup is a preservation operation: an unsupported or invalid source must
/// fail closed instead of being transformed before it is captured.
pub async fn open_database_for_backup(path: &Path) -> Result<Database, DbError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));
    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(DbError::Query)?;
    let validation = async {
        validate_quick_check(&pool).await?;
        validate_restorable_database_contract(&pool).await
    }
    .await;
    if let Err(error) = validation {
        pool.close().await;
        return Err(error);
    }
    Ok(Database { pool })
}

async fn validate_restorable_database_contract(pool: &SqlitePool) -> Result<(), DbError> {
    crate::id_schema_contract::validate_id_schema_contract(pool).await?;
    crate::id_schema_contract::validate_id_value_contract(pool).await?;
    if let Some(violation) = sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?
    {
        let table: String = violation
            .try_get("table")
            .unwrap_or_else(|_| "<unknown>".into());
        let parent: String = violation
            .try_get("parent")
            .unwrap_or_else(|_| "<unknown>".into());
        return Err(DbError::Init(format!(
            "backup foreign_key_check failed: table={table}, parent={parent}"
        )));
    }

    let identities = sqlx::query("SELECT key, owner_user_id FROM installation_identity")
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;
    if identities.len() != 1 {
        return Err(DbError::Init(format!(
            "backup installation_identity must contain exactly one row, found {}",
            identities.len()
        )));
    }
    let key: String = identities[0].try_get("key").map_err(DbError::Query)?;
    let owner_user_id: String = identities[0]
        .try_get("owner_user_id")
        .map_err(DbError::Query)?;
    if key != "installation" {
        return Err(DbError::Init(
            "backup installation_identity contains an invalid singleton key".into(),
        ));
    }
    nomifun_common::UserId::parse(owner_user_id.clone()).map_err(|error| {
        DbError::Init(format!(
            "backup installation owner ID is not canonical: {owner_user_id}: {error}"
        ))
    })?;
    let owner_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(&owner_user_id)
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;
    if owner_rows != 1 {
        return Err(DbError::Init(format!(
            "backup installation identity references missing owner user {owner_user_id}"
        )));
    }
    Ok(())
}

/// Initialize a file-backed SQLite database.
///
/// Creates the database file and parent directories if they don't exist,
/// configures pragmas (foreign_keys, busy_timeout, journal_mode=WAL),
/// runs migrations, and ensures the canonical installation owner exists.
///
/// If an existing database belongs to the retired numeric-ID migration
/// lineage, its complete SQLite file family is quarantined as
/// `*.pre-id-v2.bak*` and a clean ID-contract-v2 database is created. This is
/// an intentional pre-release hard cut: rows with integer entity keys are not
/// imported into the new lineage. Explicit corruption-like failures retain a
/// separate recovery path.
pub async fn init_database(path: &Path) -> Result<Database, DbError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| DbError::Init(format!("Failed to create database directory: {e}")))?;
    }

    match try_init_file(path).await {
        Ok(db) => Ok(db),
        Err(e) if path.exists() && is_retired_schema_lineage_error(&e) => {
            quarantine_retired_schema_database(path, e).await
        }
        Err(e) if path.exists() && should_attempt_recovery(&e) => {
            warn!("Database initialization failed, attempting recovery: {e}");
            recover_and_retry(path, e).await
        }
        Err(e) => Err(e),
    }
}

/// Initialize an in-memory SQLite database (for testing).
///
/// Uses a single connection to ensure all queries share the same in-memory database.
/// Note: WAL journal mode is not available for in-memory databases.
pub async fn init_database_memory() -> Result<Database, DbError> {
    init_database_memory_inner(None).await
}

/// Initialize an in-memory database with an explicitly supplied canonical
/// installation owner.
///
/// This deterministic variant exists for large integration fixtures that need
/// to thread the same owner through many rows. It never opens an existing
/// dataset and therefore cannot replace or alias a persisted owner.
#[doc(hidden)]
pub async fn init_database_memory_with_owner(
    owner_user_id: nomifun_common::UserId,
) -> Result<Database, DbError> {
    init_database_memory_inner(Some(owner_user_id.into_string())).await
}

async fn init_database_memory_inner(requested_owner_user_id: Option<String>) -> Result<Database, DbError> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .map_err(|e| DbError::Init(format!("Invalid memory connection string: {e}")))?
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));

    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(DbError::Query)?;

    // In-memory DBs are not shared across processes, so no advisory lock is
    // needed (and there is no on-disk path we could create one against).
    run_migrations(&pool).await?;
    crate::id_schema_contract::validate_id_schema_contract(&pool).await?;
    ensure_installation_owner(&pool, requested_owner_user_id.as_deref()).await?;

    info!("In-memory database initialized");
    Ok(Database { pool })
}

async fn try_init_file(path: &Path) -> Result<Database, DbError> {
    // Serialize the whole file-backed startup path, not only the sqlx
    // migrator. Opening a fresh SQLite file also runs connection-level PRAGMAs
    // such as WAL setup, which can race before migrations start.
    let lock_path = migrate_lock_path(path);
    let _guard = match MigrateLockGuard::acquire(&lock_path) {
        Ok(guard) => Some(guard),
        Err(e) => {
            // Don't fail startup if flock isn't available (e.g. on some
            // network filesystems) - fall back to SQLite busy-timeout and
            // retry-on-conflict behavior below.
            warn!("Could not acquire database startup lock {}: {e}", lock_path.display());
            None
        }
    };

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .journal_mode(SqliteJournalMode::Wal);

    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(opts)
        .await
        .map_err(DbError::Query)?;

    let setup = async {
        run_migrations(&pool).await?;
        crate::id_schema_contract::validate_id_schema_contract(&pool).await?;
        ensure_installation_owner(&pool, None).await
    }
    .await;
    if let Err(e) = setup {
        // Release every file handle before bubbling up so the caller can
        // rename/backup the database file (Windows refuses to rename files
        // with open handles).
        pool.close().await;
        return Err(e);
    }

    info!("Database initialized at {}", path.display());
    Ok(Database { pool })
}

/// Path of the cross-process advisory lock file used to serialize concurrent
/// migrators on the same database.
///
/// We put it next to the DB file so it lives on the same filesystem (avoids
/// odd flock semantics across mount points) and gets cleaned up alongside the
/// DB if a user resets their data directory.
fn migrate_lock_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let new_name = match p.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.migrate.lock"),
        None => "nomifun.migrate.lock".to_string(),
    };
    p.set_file_name(new_name);
    p
}

fn retry_startup_file_op<T, F>(operation: &str, path: &Path, mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    for (attempt, delay) in STARTUP_FILE_RETRY_DELAYS.iter().enumerate() {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable_startup_file_error(&e) => {
                warn!(
                    operation,
                    path = %path.display(),
                    attempt = attempt + 1,
                    retry_after_ms = delay.as_millis(),
                    raw_os_error = ?e.raw_os_error(),
                    error = %e,
                    "Startup file operation failed; retrying"
                );
                std::thread::sleep(*delay);
            }
            Err(e) => return Err(e),
        }
    }
    op()
}

fn is_retryable_startup_file_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock => true,
        _ => matches!(error.raw_os_error(), Some(5 | 32 | 33)),
    }
}

async fn run_migrations(pool: &SqlitePool) -> Result<(), DbError> {
    // File-backed callers hold a cross-process startup lock before opening the
    // SQLite pool. sqlx-sqlite's Migrate impl has no-op
    // lock()/unlock() and the migrator does list_applied -> apply without an
    // outer transaction, so two processes opening the same DB simultaneously
    // (e.g. an auto-update spawning the new version while the old one is
    // still shutting down, or `nomicore doctor` racing the server) can both
    // decide to apply the same version and the slower one's INSERT into
    // `_sqlx_migrations` blows up with `UNIQUE constraint failed:
    // _sqlx_migrations.version`. The outer startup lock also covers
    // connection PRAGMAs before migration execution.
    //
    // Any future table-rebuild migration (CREATE new + INSERT SELECT + DROP
    // old + ALTER RENAME) needs two pragmas:
    // - foreign_keys=OFF: prevents DROP TABLE from triggering ON DELETE CASCADE
    // - legacy_alter_table=ON: prevents ALTER TABLE RENAME from rewriting FK
    //   references in other tables (SQLite 3.26+ rewrites them by default)
    // Both must be set outside a transaction (sqlx wraps each migration in
    // one), so they are applied here for every migration run.
    let mut conn = pool.acquire().await.map_err(DbError::Query)?;
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query)?;

    let migration_result = run_migrations_with_retry(&mut conn).await;

    let pragma_result = sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query);

    migration_result?;
    pragma_result?;

    if let Some(violation) = sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::Query)?
    {
        let table: String = violation.try_get("table").unwrap_or_else(|_| "<unknown>".into());
        let parent: String = violation.try_get("parent").unwrap_or_else(|_| "<unknown>".into());
        let row_id: Option<i64> = violation.try_get("rowid").ok();
        return Err(DbError::Init(format!(
            "post-migration foreign_key_check failed: table={table}, rowid={row_id:?}, parent={parent}"
        )));
    }
    validate_quick_check_on_connection(&mut conn).await
}

async fn validate_quick_check(pool: &SqlitePool) -> Result<(), DbError> {
    let rows: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;
    require_quick_check_ok(rows)
}

async fn validate_quick_check_on_connection(
    conn: &mut sqlx::SqliteConnection,
) -> Result<(), DbError> {
    let rows: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::Query)?;
    require_quick_check_ok(rows)
}

fn require_quick_check_ok(rows: Vec<String>) -> Result<(), DbError> {
    if rows.len() == 1 && rows[0] == "ok" {
        return Ok(());
    }
    Err(DbError::Init(format!(
        "post-migration SQLite quick_check failed: {}",
        rows.join("; ")
    )))
}

/// Run sqlx migrations with bounded retries for known recoverable failures.
///
/// The advisory file lock above already serialises well-behaved processes, but
/// a `_sqlx_migrations` UNIQUE conflict can still leak through when:
/// - flock() failed (network FS, sandbox restrictions) and we proceeded.
/// - Two processes that both bypassed the lock raced.
///
/// In every UNIQUE-conflict scenario the failing migration's transaction was
/// rolled back, so re-running `sqlx::migrate!().run` is safe: the second
/// pass sees the row that the winner committed, checksum matches (same
/// shipped binary), and the migration is treated as already applied.
///
/// Migration 003 also has one narrowly detected compatibility retry for the
/// disabled-foreign-key behavior documented by
/// [`prepare_pending_local_model_decommission`].
async fn run_migrations_with_retry(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    let mut retried_unique_conflict = false;
    let mut retried_local_model_cleanup = false;

    loop {
        match DB_MIGRATOR.run(&mut *conn).await {
            Ok(()) => return Ok(()),
            Err(e)
                if !retried_unique_conflict && is_migrations_table_unique_conflict(&e) =>
            {
                retried_unique_conflict = true;
                warn!(
                    "Concurrent migrator detected (UNIQUE conflict on _sqlx_migrations); retrying"
                );
            }
            Err(e)
                if !retried_local_model_cleanup
                    && is_local_model_decommission_compatibility_conflict(&e)
                    && prepare_pending_local_model_decommission(conn).await? =>
            {
                retried_local_model_cleanup = true;
                warn!(
                    "Migration 003 encountered a retired local-model template binding; \
                     removed the retired live configuration and retrying"
                );
            }
            Err(e) => return Err(DbError::Migration(e)),
        }
    }
}

/// Detect the specific "another process inserted this version first" error.
///
/// sqlx wraps the SQLite error inside `MigrateError::Execute(sqlx::Error)`.
/// We match on the textual message rather than the SQLite extended error code
/// because sqlx loses the structured code by the time it bubbles up here.
fn is_migrations_table_unique_conflict(err: &sqlx::migrate::MigrateError) -> bool {
    let msg = err.to_string();
    msg.contains("UNIQUE constraint failed: _sqlx_migrations.version")
}

fn is_local_model_decommission_compatibility_conflict(
    err: &sqlx::migrate::MigrateError,
) -> bool {
    let message = err.to_string();
    message.contains("provider is still referenced by an executable Agent binding")
        || message.contains(
            "Conversation execution template must be executable, owner-scoped, and contain the lead model",
        )
}

/// Migration 003 deletes local-model templates before deleting their provider.
/// With foreign keys disabled, template participants do not cascade away and
/// conversation template references are not set to NULL. Existing integrity
/// triggers can therefore abort the migration before it reaches its final
/// foreign-key check. This compatibility path runs only after one of those
/// exact trigger failures and only while 003 is still pending, then lets the
/// immutable shipped migration retry normally.
async fn prepare_pending_local_model_decommission(
    conn: &mut sqlx::SqliteConnection,
) -> Result<bool, DbError> {
    let migration_applied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 3 AND success = 1")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    if migration_applied != 0 {
        return Ok(false);
    }

    let local_provider_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM providers WHERE platform = 'nomifun-local-model'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    if local_provider_count == 0 {
        return Ok(false);
    }

    let mut transaction = conn.begin().await.map_err(DbError::Query)?;
    sqlx::query(
        "CREATE TEMP TABLE pending_local_model_templates (id TEXT PRIMARY KEY NOT NULL)",
    )
    .execute(&mut *transaction)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO pending_local_model_templates (id) \
         SELECT DISTINCT participant.template_id \
         FROM agent_execution_template_participants participant \
         JOIN providers provider ON provider.id = participant.provider_id \
         WHERE provider.platform = 'nomifun-local-model'",
    )
    .execute(&mut *transaction)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "UPDATE conversations SET execution_template_id = NULL \
         WHERE execution_template_id IN (SELECT id FROM pending_local_model_templates)",
    )
    .execute(&mut *transaction)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "DELETE FROM agent_execution_template_participants \
         WHERE template_id IN (SELECT id FROM pending_local_model_templates)",
    )
    .execute(&mut *transaction)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "DELETE FROM agent_execution_templates \
         WHERE id IN (SELECT id FROM pending_local_model_templates)",
    )
    .execute(&mut *transaction)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "DELETE FROM model_profiles WHERE provider_id IN (\
             SELECT id FROM providers WHERE platform = 'nomifun-local-model'\
         )",
    )
    .execute(&mut *transaction)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("DROP TABLE pending_local_model_templates")
        .execute(&mut *transaction)
        .await
        .map_err(DbError::Query)?;
    transaction.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// RAII guard that holds an exclusive file lock for the lifetime of the
/// migration run. Drop unlocks and best-effort closes the file handle.
struct MigrateLockGuard {
    file: std::fs::File,
}

impl MigrateLockGuard {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // Blocking lock via fs2 has no async variant. We're inside an async
        // context but startup blocks anyway and the critical section is
        // bounded (single-process migration run), so this is acceptable.
        FileExt::lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for MigrateLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Ensure exactly one canonical installation owner exists.
///
/// The owner is a normal `user_<uuidv7>` entity. The singleton
/// `installation_identity` row is the durable indirection used by
/// repositories and SQL triggers; restoring a database therefore preserves
/// the same owner ID, while a fresh dataset mints an unrelated one.
async fn ensure_installation_owner(
    pool: &SqlitePool,
    requested_owner_user_id: Option<&str>,
) -> Result<String, DbError> {
    let mut transaction = pool.begin().await.map_err(DbError::Query)?;

    let existing: Option<String> = sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity WHERE key = 'installation'",
    )
    .fetch_optional(&mut *transaction)
    .await
    .map_err(DbError::Query)?;

    let owner_user_id = if let Some(owner_user_id) = existing {
        if let Some(requested_owner_user_id) = requested_owner_user_id
            && requested_owner_user_id != owner_user_id
        {
            return Err(DbError::Init(format!(
                "existing installation owner {owner_user_id} does not match requested test owner {requested_owner_user_id}"
            )));
        }
        nomifun_common::UserId::parse(owner_user_id.clone()).map_err(|error| {
            DbError::Init(format!(
                "installation owner ID is not canonical: {owner_user_id}: {error}"
            ))
        })?;
        let owner_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = ?")
            .bind(&owner_user_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(DbError::Query)?;
        if owner_exists != 1 {
            return Err(DbError::Init(format!(
                "installation identity references missing owner user {owner_user_id}"
            )));
        }
        owner_user_id
    } else {
        let identity_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM installation_identity")
            .fetch_one(&mut *transaction)
            .await
            .map_err(DbError::Query)?;
        if identity_rows != 0 {
            return Err(DbError::Init(
                "installation_identity contains an invalid singleton key".to_owned(),
            ));
        }
        let owner_user_id = requested_owner_user_id
            .map(str::to_owned)
            .unwrap_or_else(|| nomifun_common::UserId::new().into_string());
        nomifun_common::UserId::parse(owner_user_id.clone()).map_err(|error| {
            DbError::Init(format!(
                "requested installation owner ID is not canonical: {owner_user_id}: {error}"
            ))
        })?;
        let now = nomifun_common::now_ms();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES (?, 'admin', '', ?, ?)",
        )
        .bind(&owner_user_id)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(DbError::Query)?;
        sqlx::query(
            "INSERT INTO installation_identity (key, owner_user_id) \
             VALUES ('installation', ?)",
        )
        .bind(&owner_user_id)
        .execute(&mut *transaction)
        .await
        .map_err(DbError::Query)?;
        owner_user_id
    };

    transaction.commit().await.map_err(DbError::Query)?;
    Ok(owner_user_id)
}

// ---------------------------------------------------------------------------
// Pre-ID-v2 bootstrap salvage
//
// NOTE: pre-launch convenience; remove before release, restoring fail-fast.
//
// The 2026-06-12 clean-baseline refactor squashed migrations 001-021 into a
// single 001_baseline.sql, resetting the migration chain. Any dev database
// created before the squash fails sqlx validation: version 1's checksum no
// longer matches, and applied versions 2-021 are missing from the resolved
// set. The system has not shipped and every dev database is disposable, so
// instead of making each machine delete the file by hand we rename the old
// database (plus its -wal/-shm sidecars) to `*.pre-id-v2.bak` and rebuild
// an empty database from the baseline.
// ---------------------------------------------------------------------------

/// Classify migration failures caused by a database whose `_sqlx_migrations`
/// history does not line up with the shipped (squashed) migration set.
fn is_retired_schema_lineage_error(err: &DbError) -> bool {
    use sqlx::migrate::MigrateError;
    matches!(
        err,
        DbError::Migration(
            MigrateError::VersionMismatch(_)
                | MigrateError::VersionMissing(_)
                | MigrateError::VersionTooOld(_, _)
                | MigrateError::VersionTooNew(_, _)
        )
    )
}

/// `{file_name}.pre-id-v2.bak`, with a numeric suffix when a previous
/// backup already occupies the name.
fn retired_schema_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("nomifun-backend.db");
    let base = path.with_file_name(format!("{file_name}.pre-id-v2.bak"));
    if !base.exists() {
        return base;
    }
    for n in 1..10_000 {
        let candidate = path.with_file_name(format!("{file_name}.pre-id-v2.bak.{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Practically unreachable; fall back to a timestamped name.
    path.with_file_name(format!(
        "{file_name}.pre-id-v2.bak.{}",
        nomifun_common::now_ms()
    ))
}

/// `{file_name}{suffix}` next to `path` (SQLite sidecars append to the full
/// file name: `nomifun-backend.db-wal`, `nomifun-backend.db-shm`).
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

async fn quarantine_retired_schema_database(path: &Path, original_error: DbError) -> Result<Database, DbError> {
    let backup = retired_schema_backup_path(path);
    info!(
        db = %path.display(),
        backup = %backup.display(),
        original_error = %original_error,
        "Database uses the retired numeric-ID migration lineage; \
         renaming it aside and rebuilding an empty database \
         (pre-launch behavior -> dev databases are disposable)"
    );

    retry_startup_file_op("rename pre-id-v2 database", path, || {
        std::fs::rename(path, &backup)
    })
    .map_err(|e| {
        DbError::Init(format!(
            "Pre-ID-v2 rebuild failed: could not rename old database to {}: {e}. \
             Original error: {original_error}",
            backup.display()
        ))
    })?;

    // Move the WAL/SHM sidecars alongside the renamed database so the new
    // file does not start life next to a stale journal.
    for suffix in ["-wal", "-shm"] {
        let src = sibling_with_suffix(path, suffix);
        if !src.exists() {
            continue;
        }
        let dst = sibling_with_suffix(&backup, suffix);
        if let Err(rename_err) =
            retry_startup_file_op("rename pre-id-v2 sidecar", &src, || std::fs::rename(&src, &dst))
        {
            warn!(
                sidecar = %src.display(),
                error = %rename_err,
                "Could not rename pre-id-v2 sidecar; deleting it instead"
            );
            retry_startup_file_op("remove pre-id-v2 sidecar", &src, || std::fs::remove_file(&src)).map_err(
                |e| {
                    DbError::Init(format!(
                        "Pre-ID-v2 rebuild failed: could not move or delete sidecar {}: {e}. \
                         Original error: {original_error}",
                        src.display()
                    ))
                },
            )?;
        }
    }

    match try_init_file(path).await {
        Ok(db) => {
            info!(
                backup = %backup.display(),
                "Rebuilt empty database from baseline; old database preserved at backup path"
            );
            Ok(db)
        }
        Err(retry_err) => Err(DbError::Init(format!(
            "Pre-ID-v2 rebuild failed after renaming old database to {}: {retry_err}. \
             Original error: {original_error}",
            backup.display()
        ))),
    }
}

async fn recover_and_retry(path: &Path, original_error: DbError) -> Result<Database, DbError> {
    let backup_path = format!("{}.backup.{}", path.display(), nomifun_common::now_ms());
    warn!("Backing up corrupted database to: {backup_path}");

    std::fs::rename(path, &backup_path).map_err(|e| {
        DbError::Init(format!(
            "Recovery failed: could not backup corrupted database: {e}. \
             Original error: {original_error}"
        ))
    })?;

    match try_init_file(path).await {
        Ok(db) => {
            warn!("Database recovered. Backup at: {backup_path}");
            Ok(db)
        }
        Err(retry_err) => Err(DbError::Init(format!(
            "Recovery failed after backup: {retry_err}. Original error: {original_error}"
        ))),
    }
}

fn should_attempt_recovery(err: &DbError) -> bool {
    match err {
        DbError::Migration(_) => false,
        DbError::NotFound(_) | DbError::Conflict(_) | DbError::SafetyBackup(_) => false,
        DbError::Query(_) | DbError::Init(_) => is_corruption_like_error(err),
    }
}

fn is_corruption_like_error(err: &DbError) -> bool {
    let message = err.to_string().to_ascii_lowercase();

    [
        "sqlite_corrupt",
        "database disk image is malformed",
        "file is not a database",
        "sqlite_notadb",
        "malformed database schema",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_lineage_detector_matches_version_class_errors() {
        use sqlx::migrate::MigrateError;
        for err in [
            MigrateError::VersionMismatch(1),
            MigrateError::VersionMissing(2),
            MigrateError::VersionTooOld(1, 42),
            MigrateError::VersionTooNew(42, 1),
        ] {
            assert!(is_retired_schema_lineage_error(&DbError::Migration(err)));
        }
    }

    #[test]
    fn retired_lineage_detector_rejects_other_errors() {
        let exec = DbError::Migration(sqlx::migrate::MigrateError::Execute(
            sqlx::Error::Protocol("boom".to_owned()),
        ));
        assert!(!is_retired_schema_lineage_error(&exec));
        assert!(!is_retired_schema_lineage_error(&DbError::Init("locked".into())));
    }

    #[test]
    fn startup_file_retry_handles_windows_transient_lock_errors() {
        for code in [5, 32, 33] {
            assert!(is_retryable_startup_file_error(&std::io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn startup_file_retry_rejects_non_transient_errors() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        assert!(!is_retryable_startup_file_error(&err));
    }

    #[tokio::test]
    async fn public_snapshot_includes_committed_wal_pages_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let snapshot = dir.path().join("bundle").join("main.db");
        let database = init_database(&source).await.unwrap();
        sqlx::query("CREATE TABLE snapshot_probe (value TEXT PRIMARY KEY)")
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO snapshot_probe(value) VALUES ('committed')")
            .execute(database.pool())
            .await
            .unwrap();
        database.snapshot_into(&snapshot).await.unwrap();
        let options = SqliteConnectOptions::new()
            .filename(&snapshot)
            .create_if_missing(false)
            .read_only(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let value: String = sqlx::query_scalar("SELECT value FROM snapshot_probe")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(value, "committed");
        pool.close().await;
        assert!(database.snapshot_into(&snapshot).await.is_err());
        database.close().await;
    }
}
