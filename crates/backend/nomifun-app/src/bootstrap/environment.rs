//! Bootstrap layers shared by non-MCP subcommands.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;
use nomifun_db::sqlx::pool::PoolOptions;
use nomifun_db::sqlx::sqlite::SqliteConnectOptions;
use nomifun_db::sqlx::{Row, Sqlite, SqlitePool};
use sha2::{Digest, Sha384};
use tracing::{info, warn};

use crate::{AppConfig, config::load_or_create_storage_generation};
use nomifun_db::Database;

use crate::cli::Cli;

use super::builtin_skills::materialize_builtin_skills;
use super::server_lock::{BootServerLockAuthority, ServerLock, acquire_server_lock};
use super::tracing_init::{LogGuards, init_tracing};
use super::work_dir::resolve_work_dir;

/// Resolved environment needed by all non-MCP subcommands.
pub struct ServerEnvironment {
    /// Must be held alive for the process lifetime to flush log buffers.
    pub _log_guard: LogGuards,
    /// Exclusive per-data-dir lock; held for the process lifetime so a second
    /// backend on the same (shared-by-default) data dir fails fast instead of
    /// double-running cron/channels against the same database.
    pub _server_lock: Arc<ServerLock>,
    /// Exclusive lock for an external resolved work root.  A work root may
    /// serve more than one data directory, so the data-dir lock alone is not
    /// enough to protect `<work_dir>/conversations` from a competing reset.
    pub _work_root_lock: WorkRootLock,
    pub config: AppConfig,
}

#[derive(Debug)]
pub struct WorkRootLock {
    _file: File,
}

const WORK_ROOT_LOCK_FILE: &str = ".nomifun-work-root.lock";

fn acquire_work_root_lock(work_dir: &Path) -> Result<WorkRootLock> {
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("failed to create work dir {}", work_dir.display()))?;
    let work_metadata = std::fs::symlink_metadata(work_dir)
        .with_context(|| format!("inspect work dir {}", work_dir.display()))?;
    if work_metadata.file_type().is_symlink() || !work_metadata.is_dir() {
        anyhow::bail!(
            "resolved work dir must be a real directory: {}",
            work_dir.display()
        );
    }

    let canonical_work = std::fs::canonicalize(work_dir)
        .with_context(|| format!("canonicalize work dir {}", work_dir.display()))?;

    let path = canonical_work.join(WORK_ROOT_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open work-root lock {}", path.display()))?;
    file.try_lock_exclusive().map_err(|error| {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            anyhow::anyhow!(
                "resolved work directory {} is already in use by another NomiFun dataset; \
                 use a separate --work-dir or stop the other backend",
                canonical_work.display()
            )
        } else {
            anyhow::Error::new(error).context(format!(
                "failed to lock work root {} (filesystem without lock support?)",
                canonical_work.display()
            ))
        }
    })?;
    Ok(WorkRootLock { _file: file })
}

/// Layer 1: Logging + config resolution.
///
/// Cheap, synchronous, no IO beyond creating the log directory.
/// All subcommands that need logging and config should call this first.
pub fn init_environment(cli: &Cli, merged_path: &str) -> Result<ServerEnvironment> {
    let log_dir = cli.log_dir.clone().unwrap_or_else(|| cli.data_dir.join("logs"));
    // Export the *actual* log dir so `nomifun_system::sysinfo::resolve_log_dir`
    // (which the settings UI reads via GET /api/system/info) reports where logs
    // truly land instead of its own independent default — otherwise the UI shows
    // a Roaming path while logs write under the Local data dir. Mirrors the
    // NOMIFUN_WORK_DIR export below.
    // SAFETY: called at the very start of boot, before any service initialization
    // or env reads; the only reader of NOMIFUN_LOG_DIR is sysinfo, much later.
    unsafe {
        std::env::set_var("NOMIFUN_LOG_DIR", &log_dir);
    }
    let log_guard = init_tracing(&log_dir, cli.log_level.as_deref());

    // Notes recorded before tracing existed (e.g. the desktop shell's data-dir
    // relocation, which runs before this backend is even spawned): surface
    // them into the persistent log now — the earliest recordable point.
    for (level, message) in super::boot_log::drain_boot_notes() {
        match level {
            super::boot_log::BootNoteLevel::Info => info!(target: "boot", "{message}"),
            super::boot_log::BootNoteLevel::Warn => warn!(target: "boot", "{message}"),
        }
    }

    info!(
        path_segments = merged_path.split(if cfg!(windows) { ';' } else { ':' }).count(),
        path_len = merged_path.len(),
        "startup: PATH ready"
    );

    let work_dir = resolve_work_dir(cli.work_dir.clone(), &cli.data_dir);

    // SAFETY: called before any service initialization; no concurrent reads.
    unsafe {
        std::env::set_var("NOMIFUN_WORK_DIR", &work_dir);
        // Browser helpers in the agent/gateway crates resolve their default
        // browser root from this effective host data dir. This prevents a
        // custom `--data-dir` from silently leaving browser state under the
        // platform-global config directory, outside v3 reset/backup.
        std::env::set_var("NOMIFUN_DATA_DIR", &cli.data_dir);
    }

    // CLI-derived base policy: `--local` / `--insecure-no-auth` ⇒ NoAuth,
    // otherwise JWT Required. The desktop shell overrides this to
    // `TrustLocalToken` (with a per-boot secret) on its own serving path.
    let auth_policy = if cli.local {
        nomifun_auth::AuthPolicy::NoAuth
    } else {
        nomifun_auth::AuthPolicy::Required
    };

    let config = AppConfig {
        host: cli.host.clone(),
        port: cli.port,
        data_dir: cli.data_dir.clone(),
        work_dir,
        app_version: cli.app_version.clone(),
        auth_policy,
        local_trust_secret: None,
    };
    info!(
        "Running with auth policy {:?} — authentication is {}",
        config.auth_policy,
        if config.auth_policy.is_no_auth() { "disabled" } else { "enabled" }
    );

    // Fail fast BEFORE any data-layer work if another backend already owns
    // this data dir (all hosts share one default dir; see server_lock.rs).
    let server_lock = Arc::new(acquire_server_lock(&config.data_dir)?);
    let work_root_lock = acquire_work_root_lock(&config.work_dir)?;

    Ok(ServerEnvironment {
        _log_guard: log_guard,
        _server_lock: server_lock,
        _work_root_lock: work_root_lock,
        config,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum ExistingV3DatabaseProbe {
    Missing,
    Current,
    Incompatible(String),
}

#[derive(Debug, PartialEq, Eq)]
enum V3DataLayerState {
    FinalizedCurrent,
    BootstrapRequired,
}

const V3_BASELINE_SQL: &[u8] =
    include_bytes!("../../../nomifun-db/migrations/001_v3_baseline.sql");
const DATABASE_PROBE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

fn v3_baseline_checksum() -> Vec<u8> {
    Sha384::digest(V3_BASELINE_SQL).to_vec()
}

async fn table_has_column_contract(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    declared_type: &str,
    not_null: bool,
    primary_key: bool,
) -> Result<bool> {
    // The identifiers are fixed below and never derived from external input.
    let sql = format!("PRAGMA table_info(\"{table}\")");
    let rows = nomifun_db::sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows.iter().any(|row| {
        let Ok(name) = row.try_get::<String, _>("name") else {
            return false;
        };
        let Ok(kind) = row.try_get::<String, _>("type") else {
            return false;
        };
        let Ok(row_not_null) = row.try_get::<i64, _>("notnull") else {
            return false;
        };
        let Ok(row_primary_key) = row.try_get::<i64, _>("pk") else {
            return false;
        };
        name == column
            && kind.eq_ignore_ascii_case(declared_type)
            && (row_not_null != 0) == not_null
            && (row_primary_key != 0) == primary_key
    }))
}

async fn probe_v3_database_pool(pool: &SqlitePool) -> Result<ExistingV3DatabaseProbe> {
    let required_tables: Vec<String> = nomifun_db::sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' \
           AND name IN ('_sqlx_migrations', 'users', 'installation_identity', 'agent_metadata') \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    if required_tables
        != [
            "_sqlx_migrations",
            "agent_metadata",
            "installation_identity",
            "users",
        ]
    {
        return Ok(ExistingV3DatabaseProbe::Incompatible(
            "required v3 identity tables are missing".into(),
        ));
    }

    let migrations = nomifun_db::sqlx::query(
        "SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await?;
    if migrations.len() != 1 {
        return Ok(ExistingV3DatabaseProbe::Incompatible(format!(
            "expected one v3 baseline migration row, found {}",
            migrations.len()
        )));
    }
    let migration = &migrations[0];
    let version: i64 = migration.try_get("version")?;
    let success: bool = migration.try_get("success")?;
    let checksum: Vec<u8> = migration.try_get("checksum")?;
    if version != 1 || !success || checksum != v3_baseline_checksum() {
        return Ok(ExistingV3DatabaseProbe::Incompatible(
            "database migration lineage is not the exact v3 baseline".into(),
        ));
    }
    if let Err(error) = nomifun_db::validate_id_schema_contract(pool).await {
        return Ok(ExistingV3DatabaseProbe::Incompatible(format!(
            "database does not satisfy the complete v3 ID schema contract: {error}"
        )));
    }
    if let Err(error) = nomifun_db::validate_id_data_contract(pool).await {
        return Ok(ExistingV3DatabaseProbe::Incompatible(format!(
            "database does not satisfy the complete v3 ID data contract: {error}"
        )));
    }

    let schema_matches = table_has_column_contract(pool, "users", "id", "INTEGER", false, true)
        .await?
        && table_has_column_contract(pool, "users", "user_id", "TEXT", true, false).await?
        && table_has_column_contract(
            pool,
            "installation_identity",
            "id",
            "INTEGER",
            false,
            true,
        )
        .await?
        && table_has_column_contract(
            pool,
            "installation_identity",
            "singleton_key",
            "TEXT",
            true,
            false,
        )
        .await?
        && table_has_column_contract(
            pool,
            "installation_identity",
            "owner_user_id",
            "TEXT",
            true,
            false,
        )
        .await?
        && table_has_column_contract(
            pool,
            "agent_metadata",
            "id",
            "INTEGER",
            false,
            true,
        )
        .await?
        && table_has_column_contract(
            pool,
            "agent_metadata",
            "agent_id",
            "TEXT",
            true,
            false,
        )
        .await?;
    if !schema_matches {
        return Ok(ExistingV3DatabaseProbe::Incompatible(
            "core database identity columns do not match the v3 schema".into(),
        ));
    }

    let identities: Vec<(String, String)> = nomifun_db::sqlx::query_as(
        "SELECT singleton_key, owner_user_id FROM installation_identity",
    )
    .fetch_all(pool)
    .await?;
    let [(singleton_key, owner_user_id)] = identities.as_slice() else {
        return Ok(ExistingV3DatabaseProbe::Incompatible(format!(
            "expected one installation identity row, found {}",
            identities.len()
        )));
    };
    if singleton_key != "installation" {
        return Ok(ExistingV3DatabaseProbe::Incompatible(
            "installation identity singleton key is invalid".into(),
        ));
    }
    if nomifun_common::UserId::parse(owner_user_id.clone()).is_err() {
        return Ok(ExistingV3DatabaseProbe::Incompatible(
            "installation owner identity is not a canonical UUIDv7".into(),
        ));
    }
    let owner_rows: i64 =
        nomifun_db::sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE user_id = ?")
            .bind(owner_user_id)
            .fetch_one(pool)
            .await?;
    if owner_rows != 1 {
        return Ok(ExistingV3DatabaseProbe::Incompatible(
            "installation identity does not resolve to exactly one owner".into(),
        ));
    }

    Ok(ExistingV3DatabaseProbe::Current)
}

async fn probe_existing_v3_database(path: &Path) -> Result<ExistingV3DatabaseProbe> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExistingV3DatabaseProbe::Missing);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect database before v3 probe {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "database path must be a regular file before v3 probe: {}",
            path.display()
        );
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .busy_timeout(DATABASE_PROBE_BUSY_TIMEOUT);
    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("open existing database read-only for v3 probe {}", path.display()))?;
    let probe = probe_v3_database_pool(&pool)
        .await
        .with_context(|| format!("probe existing database v3 identity {}", path.display()));
    pool.close().await;
    probe
}

async fn prepare_v3_data_layer(config: &AppConfig) -> Result<V3DataLayerState> {
    // A receipt is also a binding to the resolved work root.  Never silently
    // accept a database that was finalized against another external workspace.
    // An explicit reset is the only operation allowed to change that binding.
    let receipt_status =
        nomifun_common::factory_reset::inspect_v3_dataset_receipt(
            &config.data_dir,
            &config.work_dir,
        )?;
    if receipt_status
        == nomifun_common::factory_reset::DatasetReceiptStatus::WorkRootMismatch
        && !config
            .data_dir
            .join(nomifun_common::factory_reset::V3_DATASET_RESET_REQUEST_FILE)
            .exists()
    {
        anyhow::bail!(
            "the v3 dataset receipt is bound to a different resolved work root; \
             refusing to accept the database with the current --work-dir; \
             request an explicit factory reset before changing the work root"
        );
    }

    // The filesystem gate always runs before the read-only SQLite probe, but
    // it is deliberately non-destructive when a database file exists.  The
    // app probe below is the only authority allowed to classify/retire that
    // database. Receipt-valid databases still have to prove their exact v3
    // migration lineage plus the complete schema and durable ID data contract
    // before writable initialization is allowed.
    match nomifun_common::factory_reset::prepare_v3_dataset(
        &config.data_dir,
        &config.work_dir,
    )? {
        nomifun_common::factory_reset::DatasetPreparation::ResetApplied => {
            info!(
                target: "boot",
                "v3 dataset reset prepared — retired data will not be migrated"
            );
        }
        nomifun_common::factory_reset::DatasetPreparation::Unchanged => {}
    }

    let state = match probe_existing_v3_database(&config.database_path()).await? {
        ExistingV3DatabaseProbe::Missing => V3DataLayerState::BootstrapRequired,
        ExistingV3DatabaseProbe::Current => {
            let reset_pending =
                nomifun_common::factory_reset::read_pending_v3_reset(
                    &config.data_dir,
                    &config.work_dir,
                )?
                .is_some();
            if !reset_pending
                && receipt_status
                    != nomifun_common::factory_reset::DatasetReceiptStatus::Current
            {
                let bootstrap_status =
                    nomifun_common::factory_reset::inspect_v3_dataset_bootstrap_binding(
                        &config.data_dir,
                        &config.work_dir,
                    )?;
                if bootstrap_status
                    == nomifun_common::factory_reset::DatasetReceiptStatus::WorkRootMismatch
                {
                    anyhow::bail!(
                        "the valid v3 database has an unfinished bootstrap binding for a \
                         different resolved work root; refusing to attach the current workspace"
                    );
                }
                if bootstrap_status
                    != nomifun_common::factory_reset::DatasetReceiptStatus::Current
                {
                    anyhow::bail!(
                        "the v3 database passed its identity probe but has neither a matching \
                         finalized receipt nor an unfinished bootstrap binding for this resolved \
                         work root; refusing to guess the workspace identity"
                    );
                }
            }
            if nomifun_common::factory_reset::require_current_v3_dataset_for_work_dir(
                &config.data_dir,
                &config.work_dir,
            )
            .is_ok()
            {
                V3DataLayerState::FinalizedCurrent
            } else {
                // A fresh database may already exist during crash recovery,
                // but the pending reset/receipt hand-off still requires the
                // full server bootstrap before it can be finalized.
                V3DataLayerState::BootstrapRequired
            }
        }
        ExistingV3DatabaseProbe::Incompatible(reason) => {
            warn!(
                target: "boot",
                database = %config.database_path().display(),
                reason,
                "database failed the pre-open v3 identity probe; retiring the claimed dataset"
            );
            nomifun_common::factory_reset::retire_non_v3_dataset_after_probe(
                &config.data_dir,
                &config.work_dir,
            )?;
            V3DataLayerState::BootstrapRequired
        }
    };
    Ok(state)
}

fn install_storage_generation_environment(config: &AppConfig) -> Result<()> {
    // Generate this only after every reset decision has removed the old
    // dataset marker and the caller has committed to bootstrapping/opening the
    // data layer. Browser-local state is outside SQLite, so the value scopes
    // every entity cache key to exactly this post-reset generation.
    let storage_generation = load_or_create_storage_generation(&config.data_dir)?;
    // SAFETY: initialization is still single-threaded and happens before any
    // service or route can read this variable.
    unsafe {
        std::env::set_var("NOMIFUN_STORAGE_GENERATION", &storage_generation);
    }
    if nomifun_common::factory_reset::inspect_v3_dataset_receipt(
        &config.data_dir,
        &config.work_dir,
    )? != nomifun_common::factory_reset::DatasetReceiptStatus::Current
    {
        nomifun_common::factory_reset::write_v3_dataset_bootstrap_binding(
            &config.data_dir,
            &config.work_dir,
            &storage_generation,
        )?;
    }
    Ok(())
}

impl ServerEnvironment {
    /// Mint authority for startup orphan reconciliation while retaining the
    /// exact OS-level server lock. This proves exclusive database ownership;
    /// it does not prove that descendants of a previous owner have exited.
    pub fn boot_reconciliation_authority(&self) -> BootServerLockAuthority {
        self._server_lock.boot_authority()
    }

    /// Open the existing finalized dataset for the doctor command.
    ///
    /// Doctor may run the destructive pre-open reset gate, but it must never
    /// create/finalize a replacement dataset without the server's complete
    /// service/side-store bootstrap.
    pub async fn init_doctor_data_layer(&self) -> Result<Database> {
        if prepare_v3_data_layer(&self.config).await?
            != V3DataLayerState::FinalizedCurrent
        {
            anyhow::bail!(
                "the v3 dataset requires bootstrap after reset; start NomiFun normally once, then rerun `nomicore doctor`"
            );
        }
        install_storage_generation_environment(&self.config)?;

        let db_path = self.config.database_path();
        info!(
            "Opening validated database for doctor at {}",
            db_path.display()
        );
        let database = nomifun_db::init_database(&db_path).await?;
        Ok(database)
    }
}

/// Layer 2: Materialize builtin skills + initialize the database.
///
/// Requires only `data_dir`. Subcommands that need persistent state
/// (database, skill files) should call this after `init_environment`.
pub async fn init_data_layer(config: &AppConfig) -> Result<Database> {
    let boot = Instant::now();

    prepare_v3_data_layer(config).await?;
    install_storage_generation_environment(config)?;

    materialize_builtin_skills(&config.data_dir).await?;
    info!(
        elapsed_ms = boot.elapsed().as_millis(),
        "startup: builtin skills materialized"
    );

    let db_path = config.database_path();
    info!("Initializing database at {}", db_path.display());
    let database = nomifun_db::init_database(&db_path).await?;
    info!(elapsed_ms = boot.elapsed().as_millis(), "startup: database initialized");

    Ok(database)
}

/// Commit the filesystem-level v3 dataset only after every required
/// product-owned side store has initialized successfully.
///
/// Keeping this separate from [`init_data_layer`] is deliberate: the main
/// SQLite schema alone is not proof that companion/public-agent/workshop and
/// the other service-owned stores completed their v3 bootstrap. If service
/// assembly fails, the pending reset plan remains durable and the next boot
/// resumes instead of accepting a half-initialized dataset.
pub fn finalize_data_layer(config: &AppConfig) -> Result<()> {
    let storage_generation = load_or_create_storage_generation(&config.data_dir)?;
    nomifun_common::factory_reset::write_v3_dataset_receipt_for_work_dir(
        &config.data_dir,
        &config.work_dir,
        &storage_generation,
    )?;
    nomifun_common::factory_reset::finalize_v3_dataset_reset(
        &config.data_dir,
        &config.work_dir,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(data_dir: &Path, work_dir: &Path) -> AppConfig {
        AppConfig {
            data_dir: data_dir.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            ..AppConfig::default()
        }
    }

    #[tokio::test]
    async fn probe_accepts_database_created_from_the_exact_v3_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        database.close().await;

        assert_eq!(
            probe_existing_v3_database(&path).await.unwrap(),
            ExistingV3DatabaseProbe::Current
        );
    }

    #[tokio::test]
    async fn finalized_current_database_is_ready_for_doctor() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        database.close().await;
        let generation = uuid::Uuid::now_v7().to_string();
        std::fs::write(data.path().join("storage-generation"), &generation).unwrap();
        nomifun_common::factory_reset::write_v3_dataset_receipt(data.path(), &generation)
            .unwrap();

        assert_eq!(
            prepare_v3_data_layer(&test_config(data.path(), data.path()))
                .await
                .unwrap(),
            V3DataLayerState::FinalizedCurrent
        );
        assert!(path.is_file());
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RESET_DIR)
                .exists()
        );
    }

    #[tokio::test]
    async fn probe_rejects_forged_v3_lineage_when_core_schema_is_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nomifun-backend.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = PoolOptions::<Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        nomifun_db::sqlx::query(
            "CREATE TABLE _sqlx_migrations (\
                version BIGINT PRIMARY KEY, \
                description TEXT NOT NULL, \
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                success BOOLEAN NOT NULL, \
                checksum BLOB NOT NULL, \
                execution_time BIGINT NOT NULL\
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        nomifun_db::sqlx::query(
            "INSERT INTO _sqlx_migrations \
                 (version, description, success, checksum, execution_time) \
             VALUES (1, 'v3 baseline', 1, ?, 0)",
        )
        .bind(v3_baseline_checksum())
        .execute(&pool)
        .await
        .unwrap();
        nomifun_db::sqlx::query(
            "CREATE TABLE users (id TEXT PRIMARY KEY, user_id TEXT NOT NULL UNIQUE)",
        )
        .execute(&pool)
        .await
        .unwrap();
        nomifun_db::sqlx::query(
            "CREATE TABLE installation_identity (\
                id TEXT PRIMARY KEY, \
                singleton_key TEXT NOT NULL, \
                owner_user_id TEXT NOT NULL\
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        nomifun_db::sqlx::query(
            "CREATE TABLE agent_metadata (id TEXT PRIMARY KEY, agent_id TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        assert!(matches!(
            probe_existing_v3_database(&path).await.unwrap(),
            ExistingV3DatabaseProbe::Incompatible(reason)
                if reason.contains("complete v3 ID schema contract")
        ));
    }

    #[tokio::test]
    async fn probe_rejects_v3_database_with_tampered_baseline_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        nomifun_db::sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
            .execute(database.pool())
            .await
            .unwrap();
        database.close().await;

        assert!(matches!(
            probe_existing_v3_database(&path).await.unwrap(),
            ExistingV3DatabaseProbe::Incompatible(reason)
                if reason.contains("migration lineage")
        ));
    }

    #[tokio::test]
    async fn probe_rejects_current_lineage_with_invalid_managed_origin_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        let mut connection = database.pool().acquire().await.unwrap();
        nomifun_db::sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *connection)
            .await
            .unwrap();
        let asset_id = nomifun_common::WorkshopAssetId::new();
        nomifun_db::sqlx::query(
            "INSERT INTO workshop_assets \
                (asset_id, kind, title, tags, in_library, origin, created_at, updated_at) \
             VALUES (?, 'image', 'corrupt origin', '[]', 1, \
                     '{\"canvas_id\":null}', 1, 1)",
        )
        .bind(asset_id.as_str())
        .execute(&mut *connection)
        .await
        .unwrap();
        nomifun_db::sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);
        database.close().await;

        assert!(matches!(
            probe_existing_v3_database(&path).await.unwrap(),
            ExistingV3DatabaseProbe::Incompatible(reason)
                if reason.contains("complete v3 ID data contract")
                    && reason.contains("origin.canvas_id")
        ));
    }

    #[tokio::test]
    async fn explicit_reset_overrides_current_receipt_and_retires_managed_side_store() {
        let data = tempfile::tempdir().unwrap();
        let config = test_config(data.path(), data.path());
        let database = nomifun_db::init_database(&config.database_path()).await.unwrap();
        database.close().await;
        let generation = uuid::Uuid::now_v7().to_string();
        std::fs::write(data.path().join("storage-generation"), &generation).unwrap();
        nomifun_common::factory_reset::write_v3_dataset_receipt(data.path(), &generation)
            .unwrap();
        std::fs::create_dir_all(data.path().join("knowledge")).unwrap();
        std::fs::write(
            data.path().join("knowledge/stale-index"),
            b"pre-reset side store",
        )
        .unwrap();
        nomifun_common::factory_reset::request_v3_dataset_reset(data.path()).unwrap();

        assert_eq!(
            prepare_v3_data_layer(&config).await.unwrap(),
            V3DataLayerState::BootstrapRequired
        );
        let plan =
            nomifun_common::factory_reset::read_pending_v3_reset(data.path(), data.path())
                .unwrap()
                .expect("explicit reset must stay pending until all side stores initialize");
        assert_eq!(
            plan.reason,
            nomifun_common::factory_reset::DatasetResetReason::ExplicitFactoryReset
        );
        assert!(!config.database_path().exists());
        assert!(!data.path().join("knowledge").exists());
        assert!(
            data.path()
                .join(plan.retired_dir)
                .join("knowledge/stale-index")
                .is_file()
        );
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RECEIPT_FILE)
                .exists(),
            "the stale pre-reset receipt must be quarantined"
        );
    }

    #[tokio::test]
    async fn finalize_publishes_receipt_only_after_side_store_bootstrap_succeeds() {
        let data = tempfile::tempdir().unwrap();
        let config = test_config(data.path(), data.path());
        std::fs::write(config.database_path(), b"old database").unwrap();
        std::fs::create_dir_all(data.path().join("companion")).unwrap();
        std::fs::write(data.path().join("companion/old-state"), b"old").unwrap();
        nomifun_common::factory_reset::request_v3_dataset_reset(data.path()).unwrap();
        assert_eq!(
            prepare_v3_data_layer(&config).await.unwrap(),
            V3DataLayerState::BootstrapRequired
        );
        install_storage_generation_environment(&config).unwrap();
        let database = nomifun_db::init_database(&config.database_path()).await.unwrap();
        database.close().await;

        let side_store = data.path().join("companion/current-state");
        std::fs::create_dir_all(side_store.parent().unwrap()).unwrap();
        std::fs::write(&side_store, b"current").unwrap();
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RECEIPT_FILE)
                .exists(),
            "database and side-store bootstrap alone must not publish the final receipt"
        );
        assert!(
            data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RESET_DIR)
                .is_dir()
        );

        finalize_data_layer(&config).unwrap();

        assert!(side_store.is_file());
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RESET_DIR)
                .exists()
        );
        nomifun_common::factory_reset::require_current_v3_dataset_for_work_dir(
            data.path(),
            data.path(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn forged_receipt_retires_legacy_database_before_writable_init() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("nomifun-backend.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = PoolOptions::<Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        nomifun_db::sqlx::query("CREATE TABLE legacy_sentinel (value TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        nomifun_db::sqlx::query(
            "INSERT INTO legacy_sentinel (value) VALUES ('must-not-migrate')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let generation = uuid::Uuid::now_v7().to_string();
        std::fs::write(data.path().join("storage-generation"), &generation).unwrap();
        nomifun_common::factory_reset::write_v3_dataset_receipt(data.path(), &generation)
            .unwrap();

        assert_eq!(
            prepare_v3_data_layer(&test_config(data.path(), data.path()))
                .await
                .unwrap(),
            V3DataLayerState::BootstrapRequired
        );
        assert!(
            !path.exists(),
            "the rejected database must be retired before init_database can open it"
        );
        let plan =
            nomifun_common::factory_reset::read_pending_v3_reset(data.path(), data.path())
                .unwrap()
                .expect("probe-triggered reset must remain pending for full server bootstrap");
        let retired_database = data
            .path()
            .join(plan.retired_dir)
            .join("nomifun-backend.db");
        assert!(retired_database.is_file());

        let retired_options = SqliteConnectOptions::new()
            .filename(&retired_database)
            .create_if_missing(false)
            .read_only(true);
        let retired = PoolOptions::<Sqlite>::new()
            .max_connections(1)
            .connect_with(retired_options)
            .await
            .unwrap();
        let sentinel: String =
            nomifun_db::sqlx::query_scalar("SELECT value FROM legacy_sentinel")
                .fetch_one(&retired)
                .await
                .unwrap();
        assert_eq!(sentinel, "must-not-migrate");
        retired.close().await;
    }

    #[tokio::test]
    async fn valid_v3_database_without_receipt_is_not_retired_before_probe() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        database.close().await;
        let generation = uuid::Uuid::now_v7().to_string();
        std::fs::write(data.path().join("storage-generation"), &generation).unwrap();
        nomifun_common::factory_reset::write_v3_dataset_bootstrap_binding(
            data.path(),
            data.path(),
            &generation,
        )
        .unwrap();

        assert_eq!(
            prepare_v3_data_layer(&test_config(data.path(), data.path()))
                .await
                .unwrap(),
            V3DataLayerState::BootstrapRequired
        );
        assert!(path.is_file());
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RESET_DIR)
                .exists()
        );
    }

    #[tokio::test]
    async fn valid_v3_database_without_any_lifecycle_binding_fails_closed() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        database.close().await;
        let generation_path = data.path().join("storage-generation");
        if generation_path.exists() {
            std::fs::remove_file(&generation_path).unwrap();
        }

        let error = prepare_v3_data_layer(&test_config(data.path(), data.path()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("neither a matching finalized receipt"));
        assert!(path.is_file());
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RESET_DIR)
                .exists()
        );
    }

    #[tokio::test]
    async fn finalized_database_rejects_a_different_resolved_work_root() {
        let data = tempfile::tempdir().unwrap();
        let first_work = tempfile::tempdir().unwrap();
        let second_work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(second_work.path().join("conversations")).unwrap();
        std::fs::write(
            second_work.path().join("conversations/legacy.txt"),
            b"must-not-be-accepted",
        )
        .unwrap();

        let path = data.path().join("nomifun-backend.db");
        let database = nomifun_db::init_database(&path).await.unwrap();
        database.close().await;
        let generation = uuid::Uuid::now_v7().to_string();
        std::fs::write(data.path().join("storage-generation"), &generation).unwrap();
        nomifun_common::factory_reset::write_v3_dataset_receipt_for_work_dir(
            data.path(),
            first_work.path(),
            &generation,
        )
        .unwrap();

        let error = prepare_v3_data_layer(&test_config(data.path(), second_work.path()))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("bound to a different resolved work root")
        );
        assert!(path.is_file());
        assert!(
            second_work
                .path()
                .join("conversations/legacy.txt")
                .is_file()
        );
        assert!(
            !data
                .path()
                .join(nomifun_common::factory_reset::V3_DATASET_RESET_DIR)
                .exists()
        );
    }

    #[test]
    fn external_work_root_lock_blocks_a_second_dataset_until_drop() {
        let work = tempfile::tempdir().unwrap();

        let first = acquire_work_root_lock(work.path()).unwrap();
        let error = acquire_work_root_lock(work.path())
            .expect_err("second dataset must not share a live external work root");
        assert!(error.to_string().contains("already in use"));

        drop(first);
        acquire_work_root_lock(work.path()).unwrap();
    }

    #[test]
    fn data_dir_as_work_root_also_gets_a_work_root_lock() {
        let data = tempfile::tempdir().unwrap();
        let first = acquire_work_root_lock(data.path()).unwrap();
        let error = acquire_work_root_lock(data.path())
            .expect_err("a second dataset must not reuse the same resolved work root");
        assert!(error.to_string().contains("already in use"));
        drop(first);
    }
}
