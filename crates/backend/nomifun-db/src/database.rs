use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use fs2::FileExt;
use sqlx::migrate::Migrator;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Sqlite, SqlitePool};
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
}

/// Initialize a file-backed SQLite database.
///
/// Creates the database file and parent directories if they don't exist,
/// configures pragmas (foreign_keys, busy_timeout, journal_mode=WAL),
/// runs migrations, and ensures the system default user exists.
///
/// If initialization fails on an existing file:
/// - A database produced by the pre-baseline migration chain (the squashed
///   001–021 history) is renamed to `*.pre-baseline.bak` and rebuilt from
///   scratch (see [`rebuild_pre_baseline_database`]).
/// - Explicit corruption-like failures attempt recovery by backing up the
///   corrupted file and creating a fresh database.
/// - Everything else (other migration errors, lock contention) fails fast.
pub async fn init_database(path: &Path) -> Result<Database, DbError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| DbError::Init(format!("Failed to create database directory: {e}")))?;
    }

    match try_init_file(path).await {
        Ok(db) => Ok(db),
        Err(e) if path.exists() && is_pre_baseline_migration_error(&e) => {
            // Guardrail (this whole salvage path is pre-launch convenience — see
            // the NOTE at `rebuild_pre_baseline_database`). The rebuild WIPES the
            // database (renames it aside, starts empty), so it must NEVER run
            // against a database that still holds a real user credential: refuse,
            // preserve the file in place, and fail fast so a genuine login is
            // never silently reset to a "needs setup" state. A disposable
            // no-credential DB (fresh/dev) is still rebuilt so startup recovers.
            if database_has_real_credential(path).await {
                return Err(DbError::Init(format!(
                    "refusing to rebuild a database that still holds a real user credential \
                     (pre-baseline migration mismatch); preserved {} in place. Original error: {e}",
                    path.display()
                )));
            }
            rebuild_pre_baseline_database(path, e).await
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
    ensure_system_user(&pool).await?;

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
        ensure_system_user(&pool).await
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
    // lock()/unlock() and the migrator does list_applied → apply without an
    // outer transaction, so two processes opening the same DB simultaneously
    // (e.g. an auto-update spawning the new version while the old one is
    // still shutting down, or `nomicore doctor` racing the server) can both
    // decide to apply the same version and the slower one's INSERT into
    // `_sqlx_migrations` blows up with `UNIQUE constraint failed:
    // _sqlx_migrations.version`. The outer startup lock also covers
    // connection PRAGMAs before migration execution.
    //
    // Any future table-rebuild migration (CREATE new + INSERT…SELECT + DROP
    // old + ALTER…RENAME) needs two pragmas:
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

    let result = run_migrations_with_retry(&mut conn).await;

    sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query)?;
    result
}

/// Run sqlx migrations with one retry on `_sqlx_migrations` UNIQUE conflict.
///
/// The advisory file lock above already serialises well-behaved processes,
/// but a UNIQUE conflict can still leak through when:
/// - flock() failed (network FS, sandbox restrictions) and we proceeded.
/// - Two processes that both bypassed the lock raced.
///
/// In every UNIQUE-conflict scenario the failing migration's transaction was
/// rolled back, so re-running `sqlx::migrate!().run` is safe: the second
/// pass sees the row that the winner committed, checksum matches (same
/// shipped binary), and the migration is treated as already applied.
async fn run_migrations_with_retry(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    match DB_MIGRATOR.run(&mut *conn).await {
        Ok(()) => Ok(()),
        Err(e) if is_migrations_table_unique_conflict(&e) => {
            warn!("Concurrent migrator detected (UNIQUE conflict on _sqlx_migrations); retrying");
            DB_MIGRATOR.run(&mut *conn).await.map_err(DbError::Migration)
        }
        Err(e) => Err(DbError::Migration(e)),
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
        // Blocking lock — fs2 has no async variant. We're inside an async
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

/// Ensure the system default user exists.
///
/// Uses INSERT OR IGNORE so it is safe to call on every startup.
/// The system user has an empty password hash, which signals "needs setup".
/// Username defaults to `admin` — matches the legacy web-host login flow so
/// users upgrading from pre-M6 builds keep the same login username.
async fn ensure_system_user(pool: &SqlitePool) -> Result<(), DbError> {
    let now = nomifun_common::now_ms();
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind("system_default_user")
    .bind("admin")
    .bind("")
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pre-baseline bootstrap salvage
//
// NOTE: pre-launch convenience; remove before release, restoring fail-fast.
//
// The 2026-06-12 clean-baseline refactor squashed migrations 001–021 into a
// single 001_baseline.sql, resetting the migration chain. Any dev database
// created before the squash fails sqlx validation: version 1's checksum no
// longer matches, and applied versions 2–21 are missing from the resolved
// set. The system has not shipped and every dev database is disposable, so
// instead of making each machine delete the file by hand we rename the old
// database (plus its -wal/-shm sidecars) to `*.pre-baseline.bak` and rebuild
// an empty database from the baseline.
// ---------------------------------------------------------------------------

/// Classify migration failures caused by a database whose `_sqlx_migrations`
/// history does not line up with the shipped (squashed) migration set.
fn is_pre_baseline_migration_error(err: &DbError) -> bool {
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

/// `{file_name}.pre-baseline.bak`, with a numeric suffix when a previous
/// backup already occupies the name.
fn pre_baseline_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("nomifun-backend.db");
    let base = path.with_file_name(format!("{file_name}.pre-baseline.bak"));
    if !base.exists() {
        return base;
    }
    for n in 1..10_000 {
        let candidate = path.with_file_name(format!("{file_name}.pre-baseline.bak.{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Practically unreachable; fall back to a timestamped name.
    path.with_file_name(format!(
        "{file_name}.pre-baseline.bak.{}",
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

/// Best-effort check: does the (migration-mismatched) database at `path` still
/// contain a user with a REAL (non-empty) password?
///
/// Gates the destructive pre-baseline rebuild so it never wipes a DB holding a
/// real credential. Opens its own connection WITHOUT running migrations and
/// only runs a `SELECT COUNT`, so it works against the old schema. Uses a
/// read-write open (not read-only) so a WAL-mode database is always fully
/// readable — a false "no credential" here would let the rebuild wipe a real
/// login, which is exactly what we must prevent. The caller has already closed
/// its own pool (see `try_init_file`), so this does not contend with it.
///
/// Any open/query failure returns `false` (allow rebuild); the rebuild only
/// RENAMES the file aside (never deletes), so an unreadable DB is still kept.
async fn database_has_real_credential(path: &Path) -> bool {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));
    let pool = match PoolOptions::<Sqlite>::new().max_connections(1).connect_with(opts).await {
        Ok(p) => p,
        Err(e) => {
            warn!("pre-baseline guard: could not open {} to probe credentials: {e}", path.display());
            return false;
        }
    };
    let probe: Result<(i64,), _> =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE password_hash != '' AND password_hash IS NOT NULL")
            .fetch_one(&pool)
            .await;
    pool.close().await;
    match probe {
        Ok((count,)) => count > 0,
        Err(e) => {
            warn!("pre-baseline guard: credential probe failed on {}: {e}", path.display());
            false
        }
    }
}

async fn rebuild_pre_baseline_database(path: &Path, original_error: DbError) -> Result<Database, DbError> {
    let backup = pre_baseline_backup_path(path);
    info!(
        db = %path.display(),
        backup = %backup.display(),
        original_error = %original_error,
        "Database predates the squashed 001_baseline migration chain; \
         renaming it aside and rebuilding an empty database \
         (pre-launch behavior — dev databases are disposable)"
    );

    retry_startup_file_op("rename pre-baseline database", path, || {
        std::fs::rename(path, &backup)
    })
    .map_err(|e| {
        DbError::Init(format!(
            "Pre-baseline rebuild failed: could not rename old database to {}: {e}. \
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
            retry_startup_file_op("rename pre-baseline sidecar", &src, || std::fs::rename(&src, &dst))
        {
            warn!(
                sidecar = %src.display(),
                error = %rename_err,
                "Could not rename pre-baseline sidecar; deleting it instead"
            );
            retry_startup_file_op("remove pre-baseline sidecar", &src, || std::fs::remove_file(&src)).map_err(
                |e| {
                    DbError::Init(format!(
                        "Pre-baseline rebuild failed: could not move or delete sidecar {}: {e}. \
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
            "Pre-baseline rebuild failed after renaming old database to {}: {retry_err}. \
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
        DbError::NotFound(_) | DbError::Conflict(_) => false,
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
    fn recovery_skips_migration_version_mismatch() {
        let err = DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(1));

        assert!(
            !should_attempt_recovery(&err),
            "migration checksum mismatch must not trigger corruption recovery"
        );
    }

    #[test]
    fn recovery_skips_lock_contention_errors() {
        let err = DbError::Init("database is locked".into());

        assert!(
            !should_attempt_recovery(&err),
            "lock contention must not trigger recovery"
        );
    }

    #[test]
    fn recovery_allows_corruption_like_errors() {
        let err = DbError::Init("database disk image is malformed".into());

        assert!(
            should_attempt_recovery(&err),
            "corruption-like failures should trigger recovery"
        );
    }

    #[test]
    fn pre_baseline_detector_matches_version_class_errors() {
        use sqlx::migrate::MigrateError;

        for err in [
            MigrateError::VersionMismatch(1),
            MigrateError::VersionMissing(2),
            MigrateError::VersionTooOld(1, 21),
            MigrateError::VersionTooNew(21, 1),
        ] {
            assert!(
                is_pre_baseline_migration_error(&DbError::Migration(err)),
                "version-class migration errors should trigger pre-baseline rebuild"
            );
        }
    }

    #[test]
    fn pre_baseline_detector_rejects_other_errors() {
        let exec = DbError::Migration(sqlx::migrate::MigrateError::Execute(sqlx::Error::Protocol(
            "boom".to_string(),
        )));
        assert!(
            !is_pre_baseline_migration_error(&exec),
            "execution failures must stay fail-fast"
        );

        let init = DbError::Init("database is locked".into());
        assert!(!is_pre_baseline_migration_error(&init));
    }

    #[test]
    fn pre_baseline_backup_path_appends_numeric_suffix_when_taken() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nomifun-backend.db");

        let first = pre_baseline_backup_path(&db);
        assert_eq!(first.file_name().unwrap(), "nomifun-backend.db.pre-baseline.bak");

        std::fs::write(&first, b"taken").unwrap();
        let second = pre_baseline_backup_path(&db);
        assert_eq!(second.file_name().unwrap(), "nomifun-backend.db.pre-baseline.bak.1");

        std::fs::write(&second, b"taken").unwrap();
        let third = pre_baseline_backup_path(&db);
        assert_eq!(third.file_name().unwrap(), "nomifun-backend.db.pre-baseline.bak.2");
    }

    #[tokio::test]
    async fn migration_preserves_fk_references() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool();

        let fk_table: String = sqlx::query_scalar(
            "SELECT \"table\" FROM pragma_foreign_key_list('messages') WHERE \"from\"='conversation_id'",
        )
        .fetch_one(pool)
        .await
        .unwrap();

        assert_eq!(fk_table, "conversations");
    }

    #[tokio::test]
    async fn database_has_real_credential_distinguishes_empty_and_set_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.db");

        // Fresh DB: only the seeded system_default_user with an EMPTY password.
        {
            let db = init_database(&path).await.unwrap();
            db.close().await;
        }
        assert!(
            !database_has_real_credential(&path).await,
            "an empty-password seed must not be treated as a real credential"
        );

        // Now write a real password hash and re-probe.
        {
            let db = init_database(&path).await.unwrap();
            sqlx::query("UPDATE users SET password_hash = 'realhash' WHERE id = 'system_default_user'")
                .execute(db.pool())
                .await
                .unwrap();
            db.close().await;
        }
        assert!(
            database_has_real_credential(&path).await,
            "a non-empty password_hash must be detected so the rebuild is refused"
        );
    }

    #[test]
    fn migrations_table_unique_conflict_detected_from_message() {
        // Build the same Execute(sqlx::Error) shape that surfaces when two
        // processes race on `INSERT INTO _sqlx_migrations`. The detector has
        // to match on the textual message because the SQLite extended code
        // is not preserved on the path through MigrateError.
        let inner = sqlx::Error::Protocol("UNIQUE constraint failed: _sqlx_migrations.version".to_string());
        let err = sqlx::migrate::MigrateError::Execute(inner);
        assert!(is_migrations_table_unique_conflict(&err));
    }

    #[test]
    fn migrations_table_unique_conflict_ignores_other_errors() {
        let other = sqlx::migrate::MigrateError::VersionMismatch(1);
        assert!(!is_migrations_table_unique_conflict(&other));

        let unrelated = sqlx::migrate::MigrateError::Execute(sqlx::Error::Protocol(
            "UNIQUE constraint failed: users.username".to_string(),
        ));
        assert!(!is_migrations_table_unique_conflict(&unrelated));
    }

    #[test]
    fn migrate_lock_path_sits_next_to_db() {
        let db = Path::new("/var/lib/nomifun/nomifun-backend.db");
        let lock = migrate_lock_path(db);
        assert_eq!(lock.parent(), db.parent());
        assert_eq!(lock.file_name().unwrap(), "nomifun-backend.db.migrate.lock");
    }

    #[test]
    fn startup_file_retry_handles_windows_transient_lock_errors() {
        for code in [5, 32, 33] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(
                is_retryable_startup_file_error(&err),
                "Windows startup file error {code} should be retryable"
            );
        }
    }

    #[test]
    fn startup_file_retry_rejects_non_transient_errors() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        assert!(!is_retryable_startup_file_error(&err));
    }
}
