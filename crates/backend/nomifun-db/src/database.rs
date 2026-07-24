use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use fs2::FileExt;
use sqlx::migrate::Migrator;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Row, Sqlite, SqlitePool};
use tracing::{info, warn};

use crate::error::DbError;

/// Maximum number of connections in the pool.
const MAX_CONNECTIONS: u32 = 5;

/// SQLite busy timeout in milliseconds.
const BUSY_TIMEOUT_MS: u64 = 5000;

static DB_MIGRATOR: Migrator = sqlx::migrate!();
const V3_BASELINE_MIGRATION_VERSION: i64 = 1;

/// Compatibility result for a persisted sqlx migration lineage.
///
/// A strict prefix is safe to hand to the embedded migrator for an incremental
/// upgrade. Anything else (a gap, unknown version, failed row, or checksum
/// mismatch) is unsupported and must fail closed before writable startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationLineageStatus {
    Current,
    UpgradeRequired,
}

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

/// Open an existing v3 database for an offline snapshot without running
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
    validate_exact_v3_migration_lineage(pool).await?;
    crate::id_schema_contract::validate_id_schema_contract(pool).await?;
    crate::id_schema_contract::validate_id_data_contract(pool).await?;

    let identities =
        sqlx::query("SELECT singleton_key, owner_user_id FROM installation_identity")
            .fetch_all(pool)
            .await
            .map_err(DbError::Query)?;
    if identities.len() != 1 {
        return Err(DbError::Init(format!(
            "backup installation_identity must contain exactly one row, found {}",
            identities.len()
        )));
    }
    let key: String = identities[0]
        .try_get("singleton_key")
        .map_err(DbError::Query)?;
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
    let owner_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE user_id = ?")
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

async fn validate_exact_v3_migration_lineage(pool: &SqlitePool) -> Result<(), DbError> {
    match inspect_supported_migration_lineage(pool).await? {
        MigrationLineageStatus::Current => Ok(()),
        MigrationLineageStatus::UpgradeRequired => Err(DbError::Init(
            "database migration lineage is a supported prefix but is not fully upgraded".into(),
        )),
    }
}

/// Validate that the applied migration rows are an exact, non-empty prefix of
/// the migrations embedded in this binary.
///
/// This is intentionally less strict than the backup/restore contract:
/// startup must admit an older supported prefix so [`init_database`] can apply
/// the missing suffix. It still rejects every lineage that the migrator cannot
/// authenticate, including unknown future versions and edited checksums.
pub async fn inspect_supported_migration_lineage(
    pool: &SqlitePool,
) -> Result<MigrationLineageStatus, DbError> {
    let expected = DB_MIGRATOR.iter().collect::<Vec<_>>();
    if expected
        .first()
        .is_none_or(|migration| migration.version != V3_BASELINE_MIGRATION_VERSION)
    {
        return Err(DbError::Init(
            "v3 migration lineage must begin with the published baseline".into(),
        ));
    }

    let rows = sqlx::query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;
    if rows.is_empty() {
        return Err(DbError::Init(format!(
            "database migration lineage must begin with embedded migration {}",
            V3_BASELINE_MIGRATION_VERSION,
        )));
    }
    if rows.len() > expected.len() {
        return Err(DbError::Init(format!(
            "database migration lineage contains {} rows but this binary embeds only {}",
            rows.len(),
            expected.len(),
        )));
    }

    for (row, expected) in rows.iter().zip(expected.iter()) {
        let version: i64 = row.try_get("version").map_err(DbError::Query)?;
        let success: bool = row.try_get("success").map_err(DbError::Query)?;
        let checksum: Vec<u8> = row.try_get("checksum").map_err(DbError::Query)?;
        if version != expected.version
            || !success
            || checksum.as_slice() != expected.checksum.as_ref()
        {
            return Err(DbError::Init(format!(
                "database migration lineage does not match embedded migration {}",
                expected.version
            )));
        }
    }
    Ok(if rows.len() == expected.len() {
        MigrationLineageStatus::Current
    } else {
        MigrationLineageStatus::UpgradeRequired
    })
}

/// Initialize a file-backed SQLite database.
///
/// Creates the database file and parent directories if they don't exist,
/// configures the busy timeout and WAL journal mode, runs migrations, and
/// ensures the canonical installation owner exists. Migration-lineage errors
/// fail fast; the app bootstrap owns any explicit dataset reset.
pub async fn init_database(path: &Path) -> Result<Database, DbError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| DbError::Init(format!("Failed to create database directory: {e}")))?;
    }

    // The database crate never renames, repairs, migrates, or replaces an
    // existing dataset. The app bootstrap owns the v3 hard-reset lifecycle
    // before any pool is opened; direct callers fail closed on corruption or
    // unsupported lineage.
    try_init_file(path).await
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
    crate::id_schema_contract::validate_id_data_contract(&pool).await?;

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
        ensure_installation_owner(&pool, None).await?;
        crate::id_schema_contract::validate_id_data_contract(&pool).await
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

async fn run_migrations(pool: &SqlitePool) -> Result<(), DbError> {
    // File-backed callers hold a cross-process startup lock before opening the
    // SQLite pool. sqlx-sqlite's Migrate impl has no-op
    // lock()/unlock() and the migrator does list_applied -> apply without an
    // outer transaction, so two processes opening the same DB simultaneously
    // (e.g. an auto-update spawning the new version while the old one is
    // still shutting down, or `nomicore doctor` racing the server) can both
    // decide to apply the same version and the slower one's INSERT into
    // `_sqlx_migrations` blows up with `UNIQUE constraint failed:
    // _sqlx_migrations.version`. The outer startup lock also covers connection
    // setup before migration execution.
    let mut conn = pool.acquire().await.map_err(DbError::Query)?;
    run_migrations_with_retry(&mut conn).await?;
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
async fn run_migrations_with_retry(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    let mut retried_unique_conflict = false;

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
/// The owner is a normal UUIDv7-addressed user entity. The singleton
/// `installation_identity` row is the durable indirection used by
/// repositories and logical-reference checks; restoring a database therefore
/// preserves the same owner ID, while a fresh dataset mints an unrelated one.
async fn ensure_installation_owner(
    pool: &SqlitePool,
    requested_owner_user_id: Option<&str>,
) -> Result<String, DbError> {
    let mut transaction = pool.begin().await.map_err(DbError::Query)?;

    let existing: Option<String> = sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity \
         WHERE singleton_key = 'installation'",
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
        let owner_exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE user_id = ?")
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
            "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
             VALUES (?, 'admin', '', ?, ?)",
        )
        .bind(&owner_user_id)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(DbError::Query)?;
        sqlx::query(
            "INSERT INTO installation_identity (singleton_key, owner_user_id) \
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_snapshot_includes_committed_wal_pages_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let snapshot = dir.path().join("bundle").join("main.db");
        let database = init_database(&source).await.unwrap();
        sqlx::query(
            "INSERT INTO client_preferences (key, value, updated_at) \
             VALUES ('snapshot_probe', 'committed', ?)",
        )
            .bind(nomifun_common::now_ms())
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
        let value: String =
            sqlx::query_scalar("SELECT value FROM client_preferences WHERE key = 'snapshot_probe'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(value, "committed");
        pool.close().await;
        assert!(database.snapshot_into(&snapshot).await.is_err());
        database.close().await;
    }
}
