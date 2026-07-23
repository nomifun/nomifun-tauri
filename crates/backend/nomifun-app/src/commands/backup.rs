//! Offline backup and restore commands for `nomicore`.
//!
//! These commands intentionally do not boot the HTTP server. Backup acquires
//! the normal per-data-dir server lock before opening SQLite, which makes it
//! safe to run only when no backend instance is using the directory. Restore
//! never opens the destination database and refuses to overwrite existing
//! files.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use nomifun_db::backup_bundle::{
    BackupObjectGraph, BackupSource, create_backup_bundle_with_sources, restore_backup_data_dir,
    validate_backup_source_roots, verify_backup_bundle,
};

use crate::cli::Cli;
use crate::config::{
    DATA_ENCRYPTION_KEY_FILE, load_or_create_storage_generation,
    validate_existing_data_encryption_key,
};

/// Create a complete offline bundle from the resolved data/work directories.
pub async fn run_backup(cli: &Cli, output: PathBuf) -> Result<ExitCode> {
    let work_dir = crate::bootstrap::resolve_work_dir(cli.work_dir.clone(), &cli.data_dir);
    let manifest = create_offline_backup(&cli.data_dir, &work_dir, &output).await?;
    println!(
        "backup created: {} ({} bytes)",
        output.display(),
        manifest.files.iter().map(|file| file.bytes).sum::<u64>()
    );
    Ok(ExitCode::SUCCESS)
}

/// Restore a verified complete bundle into a fresh destination data directory.
pub async fn run_restore(bundle: PathBuf, destination_data_dir: PathBuf) -> Result<ExitCode> {
    let outcome = restore_offline_backup(&bundle, &destination_data_dir).await?;
    println!(
        "backup restored: {} (managed workspaces restored under {}; storage-generation rotated to {})",
        destination_data_dir.display(),
        destination_data_dir.join("conversations").display(),
        outcome.destination_storage_generation
    );
    println!(
        "note: start the restored installation with --data-dir {} (and no old custom --work-dir) \
         unless you intentionally relocate the restored managed workspaces",
        destination_data_dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

async fn create_offline_backup(
    data_dir: &Path,
    work_dir: &Path,
    output: &Path,
) -> Result<nomifun_db::backup_bundle::BackupManifest> {
    let source = BackupSource::new(data_dir, work_dir);
    validate_backup_source_roots(source)
        .map_err(|error| anyhow::anyhow!("validate backup source roots: {error}"))?;
    let database_path = data_dir.join("nomifun-backend.db");
    ensure_regular_source_file(&database_path, "database")?;

    // Keep the lock alive until the snapshot and manifest have been fully
    // verified. This is the same lock the server holds for its lifetime.
    let _lock = crate::bootstrap::acquire_offline_server_lock(data_dir)?;
    nomifun_common::factory_reset::require_current_v3_dataset(data_dir)
        .map_err(|error| anyhow::anyhow!("backup requires a finalized v3 dataset: {error}"))?;
    let generation_path = data_dir.join("storage-generation");
    ensure_regular_source_file(&generation_path, "storage generation")?;
    let generation = load_or_create_storage_generation(data_dir)
        .with_context(|| format!("read storage generation in {}", data_dir.display()))?;
    let encryption_key_path = data_dir.join(DATA_ENCRYPTION_KEY_FILE);
    let encryption_key_present = match fs::symlink_metadata(&encryption_key_path) {
        Ok(_) => {
            ensure_regular_source_file(&encryption_key_path, "encryption key")?;
            validate_existing_data_encryption_key(&encryption_key_path).with_context(|| {
                format!(
                    "validate persistent encryption key {}",
                    encryption_key_path.display()
                )
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let database = nomifun_db::open_database_for_backup(&database_path)
        .await
        .with_context(|| format!("open database without mutation {}", database_path.display()))?;
    let encrypted_values = database_contains_encrypted_values(&database).await;
    let encrypted_values = match encrypted_values {
        Ok(value) => value,
        Err(error) => {
            database.close().await;
            return Err(error);
        }
    };
    if !encryption_key_present && encrypted_values {
        database.close().await;
        bail!(
            "database contains encrypted credentials but {} is missing; refusing an unrestorable backup",
            encryption_key_path.display()
        );
    }
    let result = create_backup_bundle_with_sources(
        &database,
        output,
        &generation,
        BackupObjectGraph::full_database(),
        source,
    )
    .await
    .map_err(|error| anyhow::anyhow!("create backup bundle: {error}"));
    database.close().await;
    result
}

fn ensure_regular_source_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || metadata_has_reparse_point(&metadata) || !metadata.is_file() {
        bail!("{label} must be a regular file without symlink/reparse indirection: {}", path.display());
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_has_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_has_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

async fn database_contains_encrypted_values(database: &nomifun_db::Database) -> Result<bool> {
    const PROBES: &[&str] = &[
        "SELECT EXISTS(SELECT 1 FROM providers WHERE api_key_encrypted <> '' LIMIT 1)",
        "SELECT EXISTS(SELECT 1 FROM channel_plugins WHERE config <> '' LIMIT 1)",
        "SELECT EXISTS(SELECT 1 FROM remote_agents WHERE auth_token IS NOT NULL OR device_public_key IS NOT NULL OR device_private_key IS NOT NULL OR device_token IS NOT NULL LIMIT 1)",
        "SELECT EXISTS(SELECT 1 FROM connector_credentials WHERE payload_encrypted <> '' LIMIT 1)",
        "SELECT EXISTS(SELECT 1 FROM oauth_tokens WHERE access_token <> '' OR refresh_token IS NOT NULL LIMIT 1)",
    ];
    for query in PROBES {
        let present: i64 = nomifun_db::sqlx::query_scalar(query)
            .fetch_one(database.pool())
            .await
            .with_context(|| format!("inspect encrypted backup dependency with `{query}`"))?;
        if present != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn restore_offline_backup(
    bundle: &Path,
    destination_data_dir: &Path,
) -> Result<nomifun_db::backup_bundle::RestoreOutcome> {
    let manifest = verify_backup_bundle(bundle)
        .map_err(|error| anyhow::anyhow!("verify backup bundle: {error}"))?;
    prepare_restore_destination(destination_data_dir)?;

    let outcome = restore_backup_data_dir(bundle, destination_data_dir)
    .await
    .map_err(|error| anyhow::anyhow!("restore backup bundle: {error}"))?;
    debug_assert_eq!(manifest, outcome.manifest);
    Ok(outcome)
}

fn prepare_restore_destination(destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
        if metadata.file_type().is_symlink()
            || metadata_has_reparse_point(&metadata)
            || !metadata.is_dir()
        {
            bail!(
                "restore destination must be an absent or empty directory: {}",
                destination.display()
            );
        }
        let mut entries = fs::read_dir(destination)
            .with_context(|| format!("read restore destination {}", destination.display()))?;
        if entries.next().is_some() {
            bail!(
                "restore destination must be absent or empty: {}",
                destination.display()
            );
        }
        fs::remove_dir(destination).with_context(|| {
            format!(
                "remove empty restore destination before atomic install {}",
                destination.display()
            )
        })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        create_offline_backup, load_or_create_storage_generation, prepare_restore_destination,
        restore_offline_backup,
    };
    use nomifun_common::ConversationId;
    use nomifun_db::backup_bundle::verify_backup_bundle;
    use std::fs;

    #[test]
    fn restore_destination_allows_absent_and_empty_directories() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("absent");
        prepare_restore_destination(&absent).unwrap();
        let empty = root.path().join("empty");
        fs::create_dir(&empty).unwrap();
        prepare_restore_destination(&empty).unwrap();
        assert!(!empty.exists());
    }

    #[test]
    fn restore_destination_rejects_non_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("non-empty");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("existing"), b"x").unwrap();
        assert!(prepare_restore_destination(&dir).is_err());
    }

    #[tokio::test]
    async fn command_roundtrip_preserves_ids_and_rotates_generation() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let bundle = root.path().join("bundle");
        let destination = root.path().join("restored");
        fs::create_dir(&source).unwrap();
        let database_path = source.join("nomifun-backend.db");
        let database = nomifun_db::init_database(&database_path).await.unwrap();
        let installation_owner =
            nomifun_db::installation_owner_id(database.pool()).await.unwrap();
        let conversation_id = ConversationId::new().into_string();
        nomifun_db::sqlx::query(
            "INSERT INTO conversations \
             (conversation_id, user_id, name, type, extra, status, created_at, updated_at) \
             VALUES (?, ?, 'backup command', 'nomi', '{}', 'pending', 1, 1)",
        )
        .bind(&conversation_id)
        .bind(&installation_owner)
        .execute(database.pool())
        .await
        .unwrap();
        database.close().await;

        let source_generation = load_or_create_storage_generation(&source).unwrap();
        fs::write(source.join("encryption_key"), "11".repeat(32)).unwrap();
        fs::create_dir_all(source.join("companion/shared")).unwrap();
        fs::write(source.join("companion/shared/config.json"), "{}").unwrap();
        fs::create_dir_all(source.join("conversations/managed-temp-ws")).unwrap();
        fs::write(
            source.join("conversations/managed-temp-ws/result.txt"),
            "workspace",
        )
        .unwrap();
        fs::create_dir_all(source.join("logs")).unwrap();
        fs::write(source.join("logs/ignored.log"), "runtime log").unwrap();

        let manifest = create_offline_backup(&source, &source, &bundle)
            .await
            .unwrap();
        assert_eq!(manifest.source_storage_generation, source_generation);
        assert_eq!(verify_backup_bundle(&bundle).unwrap(), manifest);

        let outcome = restore_offline_backup(&bundle, &destination).await.unwrap();
        assert_ne!(
            outcome.destination_storage_generation,
            source_generation,
            "restore must rotate the dataset namespace"
        );
        let restored = nomifun_db::init_database(&destination.join("nomifun-backend.db"))
            .await
            .unwrap();
        let restored_id: String = nomifun_db::sqlx::query_scalar(
            "SELECT conversation_id FROM conversations WHERE name = 'backup command'",
        )
        .fetch_one(restored.pool())
        .await
        .unwrap();
        assert_eq!(restored_id, conversation_id);
        restored.close().await;
        assert_eq!(
            fs::read_to_string(destination.join("encryption_key")).unwrap(),
            "11".repeat(32)
        );
        assert_eq!(
            fs::read_to_string(destination.join("companion/shared/config.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(
                destination.join("conversations/managed-temp-ws/result.txt")
            )
            .unwrap(),
            "workspace"
        );
        assert!(!destination.join("logs").exists());
    }

    #[tokio::test]
    async fn backup_refuses_a_contended_server_lock() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let database = nomifun_db::init_database(&source.join("nomifun-backend.db"))
            .await
            .unwrap();
        database.close().await;
        load_or_create_storage_generation(&source).unwrap();
        let _held = crate::bootstrap::acquire_offline_server_lock(&source).unwrap();

        let error = create_offline_backup(&source, &source, &root.path().join("bundle"))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("already in use"));
    }

    #[tokio::test]
    async fn backup_refuses_encrypted_rows_without_their_persistent_key() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let database = nomifun_db::init_database(&source.join("nomifun-backend.db"))
            .await
            .unwrap();
        nomifun_db::sqlx::query(
            "INSERT INTO providers \
             (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, capabilities, created_at, updated_at) \
             VALUES ('0190f5fe-7c00-7a00-8abc-012345678901', 'openai', 'encrypted', \
                     'https://example.invalid', 'ciphertext', '[]', 1, '[]', 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
        database.close().await;
        load_or_create_storage_generation(&source).unwrap();

        let error = create_offline_backup(&source, &source, &root.path().join("bundle"))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("encryption_key"));
        assert!(!root.path().join("bundle").exists());
    }
}
