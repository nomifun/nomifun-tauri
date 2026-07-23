//! Bridge wiring the 生成引擎 (`nomifun-creation`) to the 创意工坊 asset store
//! (`nomifun-workshop`'s data dir + `nomifun-db` index), without either domain
//! crate depending on the other.
//!
//! The creation engine defines two seams — [`AssetSink`] (persist a produced
//! artifact) and [`AssetSource`] (read a task input) — and this bridge
//! implements both over the workshop asset layout:
//! `{data_dir}/workshop/assets/{id}.{ext}` files + `workshop_assets` rows.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use nomifun_common::{WorkshopAssetId, now_ms, validate_uuidv7};
#[cfg(test)]
use nomifun_common::generate_id;
use nomifun_creation::{
    AssetSink, AssetSource, CreationError, LoadedAsset, PersistAsset, TaskArtifactCleanupFailure,
    TaskArtifactIssue, TaskArtifactManifest, TaskArtifactReconcileReport, validate_artifact_payload,
};
use nomifun_db::{IWorkshopRepository, WorkshopAssetRow};
use nomifun_workshop::{MAX_ASSET_BYTES, WORKSHOP_REL_DIR};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

static BRIDGE_SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
const ORIGIN_SHA256_KEY: &str = "artifact_sha256";

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
async fn reservation_rename_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    let reservation = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .await?;
    if let Err(error) = reservation.sync_all().await {
        drop(reservation);
        let _ = tokio::fs::remove_file(target).await;
        return Err(error);
    }
    drop(reservation);
    if let Err(error) = tokio::fs::rename(source, target).await {
        let _ = tokio::fs::remove_file(target).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
async fn sync_directory_if_supported(directory: &tokio::fs::File) -> std::io::Result<()> {
    match directory.sync_all().await {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            // The complete temp file was synced before publication and the
            // caller reads it back before indexing. A few writable SMB/FUSE
            // drivers simply do not implement directory fsync.
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
async fn rename_durable(source: &Path, target: &Path) -> std::io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "asset target has no parent directory")
    })?;
    // Hold the directory handle across publication and fsync. A hard-link in
    // the same directory is an atomic no-clobber publish: unlike POSIX rename,
    // it fails if an unexpected target already exists instead of overwriting
    // it. Removing the private temp name then leaves the fresh public name.
    let directory = tokio::fs::File::open(parent).await?;
    match tokio::fs::hard_link(source, target).await {
        Ok(()) => {
            if let Err(error) = tokio::fs::remove_file(source).await {
                let _ = tokio::fs::remove_file(target).await;
                let _ = sync_directory_if_supported(&directory).await;
                return Err(error);
            }
        }
        // Never turn an unexpected existing destination into an overwrite.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Err(error),
        Err(_) => {
            // Some otherwise writable Unix filesystems (notably SMB/FUSE and
            // a number of removable-volume drivers) do not implement hard
            // links. Reserve the fresh public name with create_new, then
            // atomically replace *our own unpublished reservation* with the
            // already-synced private temp file. The DB row is inserted only
            // after this function returns, so readers can never discover the
            // short-lived reservation through the asset index.
            if let Err(error) = reservation_rename_no_replace(source, target).await {
                let _ = sync_directory_if_supported(&directory).await;
                return Err(error);
            }
        }
    }
    if let Err(error) = sync_directory_if_supported(&directory).await {
        // The caller must never index a rename whose durability could not be
        // proven. The target id is fresh, so compensating removal is safe.
        let _ = tokio::fs::remove_file(target).await;
        let _ = sync_directory_if_supported(&directory).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
async fn rename_durable(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source = source.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: both buffers are NUL-terminated UTF-16 paths and remain alive
        // for the synchronous call. No REPLACE flag: generated ids are unique,
        // so an unexpected existing destination fails closed.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|error| std::io::Error::other(format!("durable asset rename worker failed: {error}")))?
}

#[cfg(not(any(unix, windows)))]
async fn rename_durable(source: &Path, target: &Path) -> std::io::Result<()> {
    tokio::fs::rename(source, target).await
}

/// Write within the destination directory and rename only after the complete
/// payload is closed. The target name is a fresh WorkshopAssetId, so the rename
/// is replace-free on Windows as well as macOS/Linux.
async fn save_bytes_atomic(dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let target = dir.join(file);
    let seq = BRIDGE_SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".{file}.tmp.{}.{seq}", std::process::id()));
    let result = async {
        let mut handle = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .await?;
        handle.write_all(bytes).await?;
        handle.sync_all().await?;
        drop(handle);
        rename_durable(&temp, &target).await?;
        Ok(target.clone())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

/// Persists produced artifacts / reads input assets against the workshop store.
pub struct WorkshopAssetBridge {
    data_dir: PathBuf,
    repo: Arc<dyn IWorkshopRepository>,
}

impl WorkshopAssetBridge {
    pub fn new(data_dir: PathBuf, repo: Arc<dyn IWorkshopRepository>) -> Self {
        Self { data_dir, repo }
    }

    /// Resolve the trusted data root and its real, immediate Workshop child.
    /// The configured data root itself may be reached through an intentional
    /// platform alias, so it is canonicalized first; `workshop`, however, is an
    /// owned child and must never be a symlink, junction, or reparse point.
    async fn resolve_owned_workshop_dir(&self, create: bool) -> Result<Option<PathBuf>, CreationError> {
        if create {
            tokio::fs::create_dir_all(&self.data_dir)
                .await
                .map_err(|error| CreationError::new("asset_path", format!("create data directory: {error}")))?;
        }
        let data_root = match tokio::fs::canonicalize(&self.data_dir).await {
            Ok(path) => path,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CreationError::new(
                    "asset_path",
                    format!("resolve data directory: {error}"),
                ));
            }
        };
        let data_metadata = tokio::fs::metadata(&data_root)
            .await
            .map_err(|error| CreationError::new("asset_path", format!("inspect data directory: {error}")))?;
        if !data_metadata.is_dir() {
            return Err(CreationError::new("asset_path", "configured data root is not a directory"));
        }

        // Build from the canonical root, not the caller's lexical path. Using
        // create_dir (rather than create_dir_all) means an existing link gets
        // AlreadyExists and is then rejected by symlink_metadata below instead
        // of being followed while creating descendants.
        let workshop_path = data_root.join(WORKSHOP_REL_DIR);
        if create {
            match tokio::fs::create_dir(&workshop_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(CreationError::new(
                        "asset_path",
                        format!("create workshop directory: {error}"),
                    ));
                }
            }
        }
        let metadata = match tokio::fs::symlink_metadata(&workshop_path).await {
            Ok(metadata) => metadata,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CreationError::new(
                    "asset_path",
                    format!("inspect workshop directory: {error}"),
                ));
            }
        };
        if metadata_is_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(CreationError::new(
                "asset_path",
                "workshop directory must be a real directory, not a symlink, junction, or other reparse point",
            ));
        }
        let workshop = tokio::fs::canonicalize(&workshop_path)
            .await
            .map_err(|error| CreationError::new("asset_path", format!("resolve workshop directory: {error}")))?;
        if workshop.parent() != Some(data_root.as_path()) {
            return Err(CreationError::new(
                "asset_path",
                "workshop directory escapes its canonical data root",
            ));
        }
        Ok(Some(workshop))
    }

    /// Create and resolve the generated-assets directory before writing.
    ///
    /// Every owned directory is checked as a real immediate child, starting at
    /// the canonical configured data root. This closes both `workshop ->
    /// outside` and `workshop/assets -> outside` on Unix and Windows.
    async fn prepare_owned_assets_dir(&self) -> Result<PathBuf, CreationError> {
        let workshop = self
            .resolve_owned_workshop_dir(true)
            .await
            .and_then(|path| path.ok_or_else(|| CreationError::new("asset_path", "workshop directory is absent")))?;

        let assets_path = workshop.join("assets");
        match tokio::fs::create_dir(&assets_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(CreationError::new(
                    "asset_path",
                    format!("create workshop assets directory: {error}"),
                ));
            }
        }
        let metadata = tokio::fs::symlink_metadata(&assets_path)
            .await
            .map_err(|error| CreationError::new("asset_path", format!("inspect workshop assets directory: {error}")))?;
        if metadata_is_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(CreationError::new(
                "asset_path",
                "workshop assets directory must be a real directory, not a symlink, junction, or other reparse point",
            ));
        }
        let assets = tokio::fs::canonicalize(&assets_path)
            .await
            .map_err(|error| CreationError::new("asset_path", format!("resolve workshop assets directory: {error}")))?;
        if assets.parent() != Some(workshop.as_path()) {
            return Err(CreationError::new(
                "asset_path",
                "workshop assets directory escapes its canonical workshop root",
            ));
        }
        Ok(assets)
    }

    /// Validate the exact locator shape minted by this bridge and return
    /// whether it addresses the optional thumbnail subdirectory.
    fn validate_owned_asset_locator<'a>(
        asset_id: &str,
        rel_path: &'a str,
    ) -> Result<(bool, &'a str), CreationError> {
        if rel_path.contains('\0') {
            return Err(CreationError::new("asset_path", "workshop asset path contains NUL"));
        }
        let rel = Path::new(rel_path);
        let assets_parent = Path::new(WORKSHOP_REL_DIR).join("assets");
        let thumbs_parent = assets_parent.join("thumbs");
        let parent = rel.parent();
        let in_thumbs = parent == Some(thumbs_parent.as_path());
        if rel.is_absolute() || (parent != Some(assets_parent.as_path()) && !in_thumbs) {
            return Err(CreationError::new(
                "asset_path",
                format!("refusing unexpected workshop asset path '{rel_path}'"),
            ));
        }
        let file = rel.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
            CreationError::new("asset_path", format!("invalid workshop asset path '{rel_path}'"))
        })?;
        let prefix = format!("{asset_id}.");
        let Some(extension) = file.strip_prefix(&prefix) else {
            return Err(CreationError::new(
                "asset_path",
                format!("workshop path '{rel_path}' does not belong to asset '{asset_id}'"),
            ));
        };
        if extension.is_empty() || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(CreationError::new(
                "asset_path",
                format!("workshop asset path '{rel_path}' has an invalid extension"),
            ));
        }
        Ok((in_thumbs, file))
    }

    /// Resolve an existing generated asset through canonical, owned directory
    /// roots. `None` means the owned root or file is already absent; callers
    /// such as rollback may then safely remove only the database row.
    async fn resolve_owned_asset_path(
        &self,
        asset_id: &str,
        rel_path: &str,
    ) -> Result<Option<PathBuf>, CreationError> {
        let (in_thumbs, file) = Self::validate_owned_asset_locator(asset_id, rel_path)?;
        let Some(workshop) = self.resolve_owned_workshop_dir(false).await? else {
            return Ok(None);
        };

        let assets_path = workshop.join("assets");
        let metadata = match tokio::fs::symlink_metadata(&assets_path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CreationError::new(
                    "asset_path",
                    format!("inspect workshop assets directory: {error}"),
                ));
            }
        };
        if metadata_is_reparse_point(&metadata) || !metadata.is_dir() {
            return Err(CreationError::new(
                "asset_path",
                "workshop assets directory is a symlink, junction, reparse point, or non-directory",
            ));
        }
        let assets = tokio::fs::canonicalize(&assets_path)
            .await
            .map_err(|error| CreationError::new("asset_path", format!("resolve workshop assets directory: {error}")))?;
        if assets.parent() != Some(workshop.as_path()) {
            return Err(CreationError::new(
                "asset_path",
                "workshop assets directory escapes its canonical workshop root",
            ));
        }

        let parent = if in_thumbs {
            let thumbs_path = assets.join("thumbs");
            let metadata = match tokio::fs::symlink_metadata(&thumbs_path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(CreationError::new(
                        "asset_path",
                        format!("inspect workshop thumbnail directory: {error}"),
                    ));
                }
            };
            if metadata_is_reparse_point(&metadata) || !metadata.is_dir() {
                return Err(CreationError::new(
                    "asset_path",
                    "workshop thumbnail directory is a symlink, junction, reparse point, or non-directory",
                ));
            }
            let thumbs = tokio::fs::canonicalize(&thumbs_path).await.map_err(|error| {
                CreationError::new("asset_path", format!("resolve workshop thumbnail directory: {error}"))
            })?;
            if thumbs.parent() != Some(assets.as_path()) {
                return Err(CreationError::new(
                    "asset_path",
                    "workshop thumbnail directory escapes its canonical assets root",
                ));
            }
            thumbs
        } else {
            assets
        };

        let candidate = parent.join(file);
        let metadata = match tokio::fs::symlink_metadata(&candidate).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CreationError::new(
                    "asset_path",
                    format!("inspect workshop asset file: {error}"),
                ));
            }
        };
        if metadata_is_reparse_point(&metadata) || !metadata.is_file() {
            return Err(CreationError::new(
                "asset_path",
                "workshop asset locator is a symlink, reparse point, or non-regular file",
            ));
        }
        let canonical = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|error| CreationError::new("asset_path", format!("resolve workshop asset file: {error}")))?;
        if canonical.parent() != Some(parent.as_path()) {
            return Err(CreationError::new(
                "asset_path",
                "workshop asset file escapes its canonical owned directory",
            ));
        }
        Ok(Some(canonical))
    }

    async fn rollback_one(&self, id: &str) -> Result<(), CreationError> {
        WorkshopAssetId::parse(id).map_err(|error| {
            CreationError::new("asset_rollback", format!("invalid rollback asset id '{id}': {error}"))
        })?;
        let Some(row) = self
            .repo
            .get_asset(id)
            .await
            .map_err(|error| CreationError::new("asset_rollback", format!("lookup asset '{id}': {error}")))?
        else {
            return Ok(()); // idempotent: a prior rollback already removed it
        };

        // Remove bytes before the index row. If row deletion subsequently
        // fails, retrying remains safe: NotFound files are accepted and the
        // still-present row retains the path needed to finish cleanup.
        for rel_path in [row.rel_path.as_deref(), row.thumb_rel_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let Some(abs) = self.resolve_owned_asset_path(id, rel_path).await? else {
                continue;
            };
            if let Err(error) = tokio::fs::remove_file(&abs).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(CreationError::new(
                    "asset_rollback",
                    format!("remove provisional asset file '{}': {error}", abs.display()),
                ));
            }
        }
        match self.repo.delete_asset(id).await {
            Ok(()) | Err(nomifun_db::DbError::NotFound(_)) => Ok(()),
            Err(error) => Err(CreationError::new(
                "asset_rollback",
                format!("delete provisional asset row '{id}': {error}"),
            )),
        }
    }

    fn origin_creation_task_id(row: &WorkshopAssetRow) -> Option<String> {
        row.origin
            .as_deref()
            .and_then(|origin| serde_json::from_str::<Value>(origin).ok())
            .and_then(|origin| {
                origin
                    .get("creation_task_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|id| validate_uuidv7(id).is_ok())
    }

    fn origin_sha256(row: &WorkshopAssetRow) -> Option<String> {
        row.origin
            .as_deref()
            .and_then(|origin| serde_json::from_str::<Value>(origin).ok())
            .and_then(|origin| origin.get(ORIGIN_SHA256_KEY).and_then(Value::as_str).map(str::to_string))
    }

    fn stamp_origin_hash(origin: &mut Value, bytes: &[u8]) -> Result<(), CreationError> {
        let Value::Object(fields) = origin else {
            return Err(CreationError::new(
                "asset_origin",
                "generated artifact origin must be a JSON object",
            ));
        };
        fields.insert(ORIGIN_SHA256_KEY.to_string(), Value::String(sha256_hex(bytes)));
        Ok(())
    }

    fn verify_indexed_payload(row: &WorkshopAssetRow, bytes: &[u8]) -> Result<(), String> {
        let mime = row.mime.as_deref().ok_or_else(|| "asset MIME is missing".to_string())?;
        let canonical = validate_artifact_payload(bytes, mime).map_err(|error| error.message)?;
        let expected_kind = kind_for_mime(&canonical).unwrap_or("text");
        if row.kind != expected_kind {
            return Err(format!(
                "asset kind '{}' does not match validated MIME '{canonical}'",
                row.kind
            ));
        }
        if let Some(expected_hash) = Self::origin_sha256(row) {
            let actual_hash = sha256_hex(bytes);
            if actual_hash != expected_hash {
                return Err("asset SHA-256 does not match its persisted origin digest".into());
            }
        }
        Ok(())
    }

    async fn asset_is_locatable(&self, row: &WorkshopAssetRow) -> Result<(), String> {
        if let Some(rel_path) = row.rel_path.as_deref() {
            let path = self
                .resolve_owned_asset_path(&row.asset_id, rel_path)
                .await
                .map_err(|error| error.message)?;
            let path = path.ok_or_else(|| format!("asset file '{rel_path}' is not locatable"))?;
            let metadata = tokio::fs::symlink_metadata(&path)
                .await
                .map_err(|error| format!("asset file '{}' is not locatable: {error}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
                return Err(format!("asset file '{}' is not a non-empty regular file", path.display()));
            }
            if let Some(expected) = row.bytes
                && expected >= 0
                && metadata.len() != expected as u64
            {
                return Err(format!(
                    "asset file '{}' length {} does not match indexed length {expected}",
                    path.display(),
                    metadata.len()
                ));
            }
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| format!("asset file '{}' could not be read: {error}", path.display()))?;
            Self::verify_indexed_payload(row, &bytes)
        } else if row.kind == "text"
            && row.text_content.as_deref().is_some_and(|text| !text.trim().is_empty())
        {
            Self::verify_indexed_payload(row, row.text_content.as_deref().unwrap_or_default().as_bytes())
        } else {
            Err("asset has neither a locatable file nor non-empty inline text".into())
        }
    }

    async fn audit_inventory(
        &self,
        tasks: &[TaskArtifactManifest],
        cleanup_uncommitted: bool,
    ) -> Result<TaskArtifactReconcileReport, CreationError> {
        let rows = self
            .repo
            .list_all_assets()
            .await
            .map_err(|error| CreationError::new("asset_audit", format!("scan workshop assets: {error}")))?;
        let by_id = rows.iter().map(|row| (row.asset_id.as_str(), row)).collect::<HashMap<_, _>>();
        let mut issues = Vec::new();
        let mut valid_committed = HashMap::<String, HashSet<String>>::new();

        for task in tasks.iter().filter(|task| task.committed) {
            let mut reason = None;
            if task.asset_ids.is_empty() {
                reason = Some("succeeded task has no result artifacts".to_string());
            } else if task.asset_ids.iter().collect::<HashSet<_>>().len() != task.asset_ids.len() {
                reason = Some("succeeded task contains duplicate result artifact ids".to_string());
            } else {
                for asset_id in &task.asset_ids {
                    let Some(row) = by_id.get(asset_id.as_str()) else {
                        reason = Some(format!("committed asset '{asset_id}' has no workshop index record"));
                        break;
                    };
                    if Self::origin_creation_task_id(row).as_deref()
                        != Some(task.creation_task_id.as_str())
                    {
                        reason = Some(format!("committed asset '{asset_id}' does not belong to this task"));
                        break;
                    }
                    if let Err(error) = self.asset_is_locatable(row).await {
                        reason = Some(format!("committed asset '{asset_id}' is not usable: {error}"));
                        break;
                    }
                }
            }
            if let Some(reason) = reason {
                issues.push(TaskArtifactIssue {
                    creation_task_id: task.creation_task_id.clone(),
                    reason,
                });
            } else {
                valid_committed.insert(
                    task.creation_task_id.clone(),
                    task.asset_ids.iter().cloned().collect(),
                );
            }
        }

        let mut removed_assets = 0;
        let mut cleanup_failures = Vec::new();
        if cleanup_uncommitted {
            let remove = rows
                .iter()
                .filter_map(|row| {
                    let origin_task = Self::origin_creation_task_id(row)?;
                    let preserve = valid_committed
                        .get(&origin_task)
                        .is_some_and(|asset_ids| asset_ids.contains(&row.asset_id));
                    (!preserve).then(|| (row.asset_id.clone(), origin_task))
                })
                .collect::<Vec<_>>();
            for (asset_id, creation_task_id) in remove {
                match self.rollback_one(&asset_id).await {
                    Ok(()) => removed_assets += 1,
                    Err(first_error) => match self.rollback_one(&asset_id).await {
                        Ok(()) => removed_assets += 1,
                        Err(second_error) => cleanup_failures.push(TaskArtifactCleanupFailure {
                            creation_task_id: Some(creation_task_id),
                            asset_id,
                            reason: format!(
                                "cleanup failed twice: {}; retry: {}",
                                first_error.message, second_error.message
                            ),
                        }),
                    },
                }
            }
        }

        Ok(TaskArtifactReconcileReport {
            removed_assets,
            invalid_committed_tasks: issues,
            cleanup_failures,
        })
    }

    /// Persist a generated text artifact as a `kind='text'` asset row — no file,
    /// the body lives inline in `text_content` (mirrors the workshop layer's
    /// `create_text_asset` row shape). `in_library` is honored as the engine
    /// passed it; `title` is derived from the origin prompt.
    async fn persist_text(
        &self,
        bytes: Vec<u8>,
        mime: String,
        in_library: bool,
        origin: &Value,
    ) -> Result<String, CreationError> {
        let text = String::from_utf8(bytes)
            .map_err(|_| CreationError::new("invalid_artifact", "text artifact is not valid UTF-8"))?;
        let id = WorkshopAssetId::new().into_string();
        let now = now_ms();
        let row = WorkshopAssetRow {
            id: 0,
            asset_id: id.clone(),
            kind: "text".to_string(),
            title: title_from_origin(origin, &id),
            collection: None,
            tags: "[]".to_string(),
            rel_path: None,
            thumb_rel_path: None,
            mime: Some(mime),
            width: None,
            height: None,
            bytes: None,
            text_content: Some(text),
            in_library,
            origin: serde_json::to_string(origin).ok(),
            created_at: now,
            updated_at: now,
        };
        self.repo
            .create_asset(&row)
            .await
            .map(|saved| saved.asset_id)
            .map_err(|e| CreationError::new("asset_index", format!("register text asset row: {e}")))
    }
}

#[async_trait]
impl AssetSink for WorkshopAssetBridge {
    async fn persist(&self, asset: PersistAsset) -> Result<String, CreationError> {
        let PersistAsset { bytes, mime, in_library, mut origin } = asset;

        if bytes.len() > MAX_ASSET_BYTES {
            return Err(CreationError::new(
                "invalid_artifact",
                format!("artifact exceeds the {} byte workshop limit", MAX_ASSET_BYTES),
            ));
        }
        let mime = validate_artifact_payload(&bytes, &mime)?;
        Self::stamp_origin_hash(&mut origin, &bytes)?;

        // Text artifacts have no file: index them as `kind='text'` rows carrying
        // the body inline in `text_content`.
        if mime == "text/plain" {
            return self.persist_text(bytes, mime, in_library, &origin).await;
        }

        let id = WorkshopAssetId::new().into_string();
        let ext = ext_for_mime(&mime).ok_or_else(|| {
            CreationError::new("invalid_artifact", format!("unsupported workshop artifact MIME '{mime}'"))
        })?;
        let kind = kind_for_mime(&mime).ok_or_else(|| {
            CreationError::new("invalid_artifact", format!("unsupported workshop artifact MIME '{mime}'"))
        })?;
        let disk_name = format!("{id}.{ext}");
        let rel_path = format!("{WORKSHOP_REL_DIR}/assets/{disk_name}");
        let assets_dir = self.prepare_owned_assets_dir().await?;

        let byte_len = bytes.len() as i64;
        let abs = save_bytes_atomic(&assets_dir, &disk_name, &bytes)
            .await
            .map_err(|e| CreationError::new("asset_write", format!("atomically write asset file: {e}")))?;

        // Do not create a database record until the renamed destination can be
        // read back byte-for-byte. A matching length alone cannot detect a
        // corrupt or incorrectly written product.
        let verified = tokio::fs::read(&abs).await.is_ok_and(|written| written == bytes);
        if !verified {
            let _ = tokio::fs::remove_file(&abs).await;
            return Err(CreationError::new(
                "asset_write",
                "asset file verification failed after atomic write",
            ));
        }

        let origin_json = serde_json::to_string(&origin).ok();
        let now = now_ms();
        let row = WorkshopAssetRow {
            id: 0,
            asset_id: id.clone(),
            kind: kind.to_string(),
            title: title_from_origin(&origin, &id),
            collection: None,
            tags: "[]".to_string(),
            rel_path: Some(rel_path),
            thumb_rel_path: None,
            mime: Some(mime),
            width: None,  // best-effort omitted (P0); the workshop upload path fills these
            height: None,
            bytes: Some(byte_len),
            text_content: None,
            in_library,
            origin: origin_json,
            created_at: now,
            updated_at: now,
        };

        match self.repo.create_asset(&row).await {
            Ok(saved) => Ok(saved.asset_id),
            Err(e) => {
                // Roll the orphaned file back on insert failure.
                let _ = tokio::fs::remove_file(&abs).await;
                Err(CreationError::new("asset_index", format!("register asset row: {e}")))
            }
        }
    }

    async fn rollback(&self, asset_ids: &[String]) -> Result<(), CreationError> {
        let mut failures = Vec::new();
        for id in asset_ids {
            if let Err(error) = self.rollback_one(id).await {
                failures.push(format!("{id}: {}", error.message));
            }
        }
        if !failures.is_empty() {
            return Err(CreationError::new(
                "asset_rollback",
                format!("one or more provisional assets could not be removed: {}", failures.join("; ")),
            ));
        }
        Ok(())
    }

    async fn verify_task_artifacts(
        &self,
        committed_tasks: &[TaskArtifactManifest],
    ) -> Result<Vec<TaskArtifactIssue>, CreationError> {
        Ok(self.audit_inventory(committed_tasks, false).await?.invalid_committed_tasks)
    }

    async fn reconcile_task_artifacts(
        &self,
        all_tasks: &[TaskArtifactManifest],
    ) -> Result<TaskArtifactReconcileReport, CreationError> {
        self.audit_inventory(all_tasks, true).await
    }
}

#[async_trait]
impl AssetSource for WorkshopAssetBridge {
    async fn load(&self, asset_id: &str) -> Result<LoadedAsset, CreationError> {
        WorkshopAssetId::parse(asset_id)
            .map_err(|error| CreationError::new("asset_id", format!("invalid input asset id: {error}")))?;
        let row = self
            .repo
            .get_asset(asset_id)
            .await
            .map_err(|e| CreationError::new("asset_lookup", format!("asset lookup failed: {e}")))?
            .ok_or_else(|| CreationError::new("asset_not_found", format!("input asset '{asset_id}' not found")))?;

        // File-backed assets (image/video) are read from disk; text assets carry
        // their body inline (`text_content`, no file) — return it as UTF-8 bytes
        // so a text asset can be reused as a prompt input.
        if let Some(rel) = row.rel_path {
            let abs = self
                .resolve_owned_asset_path(asset_id, &rel)
                .await?
                .ok_or_else(|| CreationError::new("asset_read", format!("input asset '{asset_id}' file is missing")))?;
            let bytes = tokio::fs::read(&abs)
                .await
                .map_err(|e| CreationError::new("asset_read", format!("read input asset '{asset_id}': {e}")))?;
            let mime = row.mime.unwrap_or_else(|| "application/octet-stream".to_string());
            Ok(LoadedAsset { bytes, mime })
        } else if let Some(text) = row.text_content {
            let mime = row.mime.unwrap_or_else(|| "text/plain; charset=utf-8".to_string());
            Ok(LoadedAsset { bytes: text.into_bytes(), mime })
        } else {
            Err(CreationError::new(
                "asset_no_file",
                format!("input asset '{asset_id}' has no file or text body"),
            ))
        }
    }
}

/// A short, human-ish title from the origin prompt (falls back to the asset id)
/// — the asset library shows this.
fn title_from_origin(origin: &Value, fallback_id: &str) -> String {
    origin
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(60).collect::<String>())
        .unwrap_or_else(|| fallback_id.to_string())
}

/// `image | video | audio` for the workshop asset `kind` column, from a MIME.
fn kind_for_mime(mime: &str) -> Option<&'static str> {
    if mime.starts_with("video/") {
        Some("video")
    } else if mime.starts_with("audio/") {
        Some("audio")
    } else if mime.starts_with("image/") {
        Some("image")
    } else {
        None
    }
}

/// A file extension for a supported produced-artifact MIME.
fn ext_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "video/quicktime" => Some("mov"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "audio/flac" => Some("flac"),
        "audio/mp4" => Some("m4a"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{SqliteWorkshopRepository, init_database_memory};
    use serde_json::json;

    fn png_with_pixel(pixel: [u8; 4]) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba(pixel),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn valid_png() -> Vec<u8> {
        png_with_pixel([7, 8, 9, 255])
    }

    fn valid_wav() -> Vec<u8> {
        // PCM mono, 8 kHz, 8-bit, one sample. Includes complete fmt + data
        // chunks so payload validation proves this is audio rather than merely
        // accepting the 12-byte RIFF/WAVE prefix.
        vec![
            b'R', b'I', b'F', b'F', 38, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ', 16, 0, 0, 0, 1,
            0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0, b'd', b'a', b't', b'a', 1, 0, 0, 0, 128,
            0,
        ]
    }

    #[test]
    fn mime_mappings() {
        assert_eq!(ext_for_mime("image/png"), Some("png"));
        assert_eq!(ext_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(ext_for_mime("video/mp4"), Some("mp4"));
        assert_eq!(ext_for_mime("audio/wav"), Some("wav"));
        assert_eq!(ext_for_mime("application/pdf"), None);
        assert_eq!(kind_for_mime("image/png"), Some("image"));
        assert_eq!(kind_for_mime("video/mp4"), Some("video"));
        assert_eq!(kind_for_mime("audio/wav"), Some("audio"));
        assert_eq!(kind_for_mime("application/octet-stream"), None);
    }

    #[test]
    fn generated_asset_locators_are_exact_and_traversal_free() {
        let id = WorkshopAssetId::new().into_string();
        let direct = format!("{WORKSHOP_REL_DIR}/assets/{id}.png");
        let thumb = format!("{WORKSHOP_REL_DIR}/assets/thumbs/{id}.jpg");
        assert_eq!(
            WorkshopAssetBridge::validate_owned_asset_locator(&id, &direct).unwrap(),
            (false, direct.rsplit('/').next().unwrap())
        );
        assert_eq!(
            WorkshopAssetBridge::validate_owned_asset_locator(&id, &thumb).unwrap(),
            (true, thumb.rsplit('/').next().unwrap())
        );

        for invalid in [
            format!("{WORKSHOP_REL_DIR}/assets/../outside/{id}.png"),
            format!("{WORKSHOP_REL_DIR}/assets/other.png"),
            format!("{WORKSHOP_REL_DIR}/assets/{id}.png:stream"),
            format!("other/assets/{id}.png"),
            format!("/{WORKSHOP_REL_DIR}/assets/{id}.png"),
        ] {
            assert!(
                WorkshopAssetBridge::validate_owned_asset_locator(&id, &invalid).is_err(),
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn title_from_origin_truncates_or_falls_back() {
        let fallback_id = WorkshopAssetId::new().into_string();
        assert_eq!(title_from_origin(&json!({"prompt": "a fox"}), &fallback_id), "a fox");
        assert_eq!(title_from_origin(&json!({"prompt": "   "}), &fallback_id), fallback_id);
        assert_eq!(title_from_origin(&json!({}), &fallback_id), fallback_id);
        let long = "x".repeat(80);
        assert_eq!(title_from_origin(&json!({"prompt": long}), &fallback_id).chars().count(), 60);
    }

    async fn bridge() -> (WorkshopAssetBridge, tempfile::TempDir, nomifun_db::Database) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IWorkshopRepository> = Arc::new(SqliteWorkshopRepository::new(db.pool().clone()));
        let dir = tempfile::tempdir().unwrap();
        let bridge = WorkshopAssetBridge::new(dir.path().to_path_buf(), repo);
        (bridge, dir, db)
    }

    #[cfg(unix)]
    fn create_test_dir_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn create_test_dir_link(target: &Path, link: &Path) {
        junction::create(target, link).unwrap();
    }

    #[cfg(unix)]
    fn remove_test_dir_link(link: &Path) {
        std::fs::remove_file(link).unwrap();
    }

    #[cfg(windows)]
    fn remove_test_dir_link(link: &Path) {
        junction::delete(link).unwrap();
    }

    #[tokio::test]
    async fn persist_text_writes_row_not_file() {
        let (bridge, dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                bytes: "generated story".as_bytes().to_vec(),
                mime: "text/plain; charset=utf-8".into(),
                in_library: true,
                origin: json!({"prompt": "write a story about a fox", "model": "gpt-4o"}),
            })
            .await
            .unwrap();
        WorkshopAssetId::parse(&id).expect("persisted asset id must be a bare UUIDv7");

        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(row.kind, "text");
        assert_eq!(row.text_content.as_deref(), Some("generated story"));
        assert_eq!(row.rel_path, None);
        assert!(row.mime.as_deref().unwrap().starts_with("text/plain"));
        assert_eq!(row.title, "write a story about a fox");
        assert!(row.in_library);
        assert!(row.origin.is_some(), "origin JSON should be stamped");

        // No file written under the assets dir (text assets are file-less).
        let assets_dir = dir.path().join("workshop").join("assets");
        let count = std::fs::read_dir(&assets_dir).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(count, 0, "text asset must not write a file");
    }

    #[tokio::test]
    async fn persist_image_atomically_writes_verified_file_before_row() {
        let (bridge, dir, _db) = bridge().await;
        let bytes = valid_png();
        let id = bridge
            .persist(PersistAsset {
                bytes: bytes.clone(),
                mime: "image/png; charset=binary".into(),
                in_library: true,
                origin: json!({"prompt": "a real image"}),
            })
            .await
            .unwrap();

        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(row.kind, "image");
        assert_eq!(row.mime.as_deref(), Some("image/png"));
        assert_eq!(row.bytes, Some(bytes.len() as i64));
        let origin: Value = serde_json::from_str(row.origin.as_deref().unwrap()).unwrap();
        assert_eq!(origin[ORIGIN_SHA256_KEY].as_str().unwrap().len(), 64);
        let rel_path = row.rel_path.expect("binary asset must have a path");
        assert_eq!(tokio::fs::read(dir.path().join(rel_path)).await.unwrap(), bytes);

        let assets_dir = dir.path().join("workshop").join("assets");
        let leftovers = std::fs::read_dir(assets_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0, "atomic write must not leave a temp product");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn persist_rejects_assets_directory_symlink_escape_without_external_write() {
        let (bridge, dir, _db) = bridge().await;
        let external = tempfile::tempdir().unwrap();
        let workshop = dir.path().join(WORKSHOP_REL_DIR);
        tokio::fs::create_dir_all(&workshop).await.unwrap();
        let assets_link = workshop.join("assets");
        create_test_dir_link(external.path(), &assets_link);

        let error = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"prompt": "must stay in workshop"}),
            })
            .await
            .unwrap_err();

        assert_eq!(error.kind, "asset_path");
        assert!(bridge.repo.list_all_assets().await.unwrap().is_empty());
        assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 0);
        remove_test_dir_link(&assets_link);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn persist_rejects_workshop_ancestor_link_escape_without_external_write() {
        let (bridge, dir, _db) = bridge().await;
        let external = tempfile::tempdir().unwrap();
        let workshop_link = dir.path().join(WORKSHOP_REL_DIR);
        create_test_dir_link(external.path(), &workshop_link);

        let error = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"prompt": "must not escape the data root"}),
            })
            .await
            .unwrap_err();

        assert_eq!(error.kind, "asset_path");
        assert!(bridge.repo.list_all_assets().await.unwrap().is_empty());
        assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 0);
        remove_test_dir_link(&workshop_link);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn rollback_refuses_ancestor_symlink_escape_and_preserves_external_file() {
        let (bridge, dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"prompt": "provisional"}),
            })
            .await
            .unwrap();
        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        let rel_path = row.rel_path.as_deref().unwrap();
        let file_name = Path::new(rel_path).file_name().unwrap();
        let assets = dir.path().join(WORKSHOP_REL_DIR).join("assets");
        tokio::fs::remove_file(assets.join(file_name)).await.unwrap();
        tokio::fs::remove_dir(&assets).await.unwrap();

        let external = tempfile::tempdir().unwrap();
        let external_file = external.path().join(file_name);
        tokio::fs::write(&external_file, b"external sentinel").await.unwrap();
        create_test_dir_link(external.path(), &assets);

        let error = bridge.rollback(std::slice::from_ref(&id)).await.unwrap_err();
        assert_eq!(error.kind, "asset_rollback");
        assert_eq!(tokio::fs::read(&external_file).await.unwrap(), b"external sentinel");
        assert!(bridge.repo.get_asset(&id).await.unwrap().is_some());
        remove_test_dir_link(&assets);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn rollback_refuses_workshop_ancestor_link_and_preserves_external_file() {
        let (bridge, dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"prompt": "provisional"}),
            })
            .await
            .unwrap();
        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        let rel_path = row.rel_path.as_deref().unwrap();
        let file_name = Path::new(rel_path).file_name().unwrap();
        let workshop = dir.path().join(WORKSHOP_REL_DIR);
        tokio::fs::remove_dir_all(&workshop).await.unwrap();

        let external = tempfile::tempdir().unwrap();
        let external_assets = external.path().join("assets");
        tokio::fs::create_dir(&external_assets).await.unwrap();
        let external_file = external_assets.join(file_name);
        tokio::fs::write(&external_file, b"external sentinel").await.unwrap();
        create_test_dir_link(external.path(), &workshop);

        let error = bridge.rollback(std::slice::from_ref(&id)).await.unwrap_err();
        assert_eq!(error.kind, "asset_rollback");
        assert_eq!(tokio::fs::read(&external_file).await.unwrap(), b"external sentinel");
        assert!(bridge.repo.get_asset(&id).await.unwrap().is_some());
        remove_test_dir_link(&workshop);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn atomic_publish_never_replaces_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("existing.bin");
        tokio::fs::write(&target, b"original bytes").await.unwrap();

        let error = save_bytes_atomic(dir.path(), "existing.bin", b"new bytes")
            .await
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"original bytes");
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0, "failed no-replace publication must remove its private temp file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsupported_hard_link_reservation_fallback_is_no_replace() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.tmp");
        let target = dir.path().join("target.bin");
        tokio::fs::write(&source, b"complete asset").await.unwrap();

        reservation_rename_no_replace(&source, &target).await.unwrap();
        assert!(!source.exists());
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"complete asset");

        let second_source = dir.path().join("second.tmp");
        tokio::fs::write(&second_source, b"must not replace").await.unwrap();
        let error = reservation_rename_no_replace(&second_source, &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"complete asset");
        assert_eq!(tokio::fs::read(&second_source).await.unwrap(), b"must not replace");
    }

    #[tokio::test]
    async fn committed_audit_rejects_same_size_valid_payload_replacement() {
        let (bridge, dir, _db) = bridge().await;
        let creation_task_id = generate_id();
        let original = valid_png();
        let replacement = png_with_pixel([200, 100, 50, 255]);
        assert_eq!(replacement.len(), original.len(), "fixture must exercise same-size replacement");
        let asset_id = bridge
            .persist(PersistAsset {
                bytes: original,
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"creation_task_id": creation_task_id.clone()}),
            })
            .await
            .unwrap();
        let row = bridge.repo.get_asset(&asset_id).await.unwrap().unwrap();
        let path = dir.path().join(row.rel_path.unwrap());
        tokio::fs::write(path, replacement).await.unwrap();

        let issues = bridge
            .verify_task_artifacts(&[TaskArtifactManifest {
                creation_task_id: creation_task_id.clone(),
                committed: true,
                asset_ids: vec![asset_id],
            }])
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].creation_task_id, creation_task_id);
        assert!(issues[0].reason.contains("SHA-256"));
    }

    #[tokio::test]
    async fn persist_audio_writes_a_verified_locatable_asset() {
        let (bridge, dir, _db) = bridge().await;
        let bytes = valid_wav();
        let id = bridge
            .persist(PersistAsset {
                bytes: bytes.clone(),
                mime: "audio/x-wav".into(),
                in_library: true,
                origin: json!({"prompt": "read this aloud"}),
            })
            .await
            .unwrap();

        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(row.kind, "audio");
        assert_eq!(row.mime.as_deref(), Some("audio/wav"));
        let rel_path = row.rel_path.expect("audio artifact must have a path");
        assert_eq!(tokio::fs::read(dir.path().join(rel_path)).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn rollback_removes_binary_and_text_assets_and_is_idempotent() {
        let (bridge, dir, _db) = bridge().await;
        let image_id = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"prompt": "provisional image"}),
            })
            .await
            .unwrap();
        let text_id = bridge
            .persist(PersistAsset {
                bytes: b"provisional text".to_vec(),
                mime: "text/plain".into(),
                in_library: true,
                origin: json!({"prompt": "provisional text"}),
            })
            .await
            .unwrap();
        let image_row = bridge.repo.get_asset(&image_id).await.unwrap().unwrap();
        let image_path = dir.path().join(image_row.rel_path.unwrap());
        assert!(image_path.exists());

        let batch = vec![image_id.clone(), text_id.clone()];
        bridge.rollback(&batch).await.unwrap();
        assert!(!image_path.exists());
        assert!(bridge.repo.get_asset(&image_id).await.unwrap().is_none());
        assert!(bridge.repo.get_asset(&text_id).await.unwrap().is_none());

        // Retry is safe after a crash/timeout obscures the first response.
        bridge.rollback(&batch).await.unwrap();
    }

    #[tokio::test]
    async fn complete_inventory_preserves_only_valid_committed_assets_and_is_idempotent() {
        let (bridge, dir, _db) = bridge().await;
        let committed_task = generate_id();
        let canceled_task = generate_id();
        let empty_success_task = generate_id();
        let missing_file_task = generate_id();
        let committed = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"creation_task_id": committed_task, "prompt": "committed"}),
            })
            .await
            .unwrap();
        let extra_same_task = bridge
            .persist(PersistAsset {
                bytes: b"uncommitted retry".to_vec(),
                mime: "text/plain".into(),
                in_library: true,
                origin: json!({"creation_task_id": committed_task}),
            })
            .await
            .unwrap();
        let canceled = bridge
            .persist(PersistAsset {
                bytes: b"canceled".to_vec(),
                mime: "text/plain".into(),
                in_library: true,
                origin: json!({"creation_task_id": canceled_task}),
            })
            .await
            .unwrap();
        let empty_success = bridge
            .persist(PersistAsset {
                bytes: b"empty manifest".to_vec(),
                mime: "text/plain".into(),
                in_library: true,
                origin: json!({"creation_task_id": empty_success_task}),
            })
            .await
            .unwrap();
        let orphan_task = generate_id();
        let orphan = bridge
            .persist(PersistAsset {
                bytes: b"missing task".to_vec(),
                mime: "text/plain".into(),
                in_library: true,
                origin: json!({"creation_task_id": orphan_task}),
            })
            .await
            .unwrap();
        let missing_file = bridge
            .persist(PersistAsset {
                bytes: valid_png(),
                mime: "image/png".into(),
                in_library: true,
                origin: json!({"creation_task_id": missing_file_task}),
            })
            .await
            .unwrap();
        let missing_file_path = dir
            .path()
            .join(bridge.repo.get_asset(&missing_file).await.unwrap().unwrap().rel_path.unwrap());
        tokio::fs::remove_file(&missing_file_path).await.unwrap();
        let unowned_upload = bridge
            .persist(PersistAsset {
                bytes: b"user-owned".to_vec(),
                mime: "text/plain".into(),
                in_library: true,
                origin: json!({"source": "upload"}),
            })
            .await
            .unwrap();

        let manifests = vec![
            TaskArtifactManifest {
                creation_task_id: committed_task.clone(),
                committed: true,
                asset_ids: vec![committed.clone()],
            },
            TaskArtifactManifest {
                creation_task_id: canceled_task.clone(),
                committed: false,
                asset_ids: vec![],
            },
            TaskArtifactManifest {
                creation_task_id: empty_success_task.clone(),
                committed: true,
                asset_ids: vec![],
            },
            TaskArtifactManifest {
                creation_task_id: missing_file_task.clone(),
                committed: true,
                asset_ids: vec![missing_file.clone()],
            },
        ];
        let report = bridge.reconcile_task_artifacts(&manifests).await.unwrap();
        assert_eq!(report.removed_assets, 5);
        let invalid = report
            .invalid_committed_tasks
            .iter()
            .map(|issue| issue.creation_task_id.clone())
            .collect::<HashSet<_>>();
        assert_eq!(invalid, HashSet::from([empty_success_task, missing_file_task]));
        assert!(bridge.repo.get_asset(&committed).await.unwrap().is_some());
        assert!(bridge.repo.get_asset(&unowned_upload).await.unwrap().is_some());
        for removed in [extra_same_task, canceled, empty_success, orphan, missing_file] {
            assert!(bridge.repo.get_asset(&removed).await.unwrap().is_none());
        }

        let second = bridge.reconcile_task_artifacts(&manifests).await.unwrap();
        assert_eq!(second.removed_assets, 0);
    }

    #[tokio::test]
    async fn invalid_binary_products_create_neither_file_nor_index_row() {
        for bytes in [Vec::new(), b"not-an-image".to_vec(), b"<!doctype html><title>error</title>".to_vec()] {
            let (bridge, dir, _db) = bridge().await;
            let result = bridge
                .persist(PersistAsset {
                    bytes,
                    mime: "image/png".into(),
                    in_library: true,
                    origin: json!({"prompt": "must fail"}),
                })
                .await;
            assert!(result.is_err());
            assert!(bridge.repo.list_all_assets().await.unwrap().is_empty());
            let assets_dir = dir.path().join("workshop").join("assets");
            let file_count = std::fs::read_dir(assets_dir).map(|entries| entries.count()).unwrap_or(0);
            assert_eq!(file_count, 0, "invalid artifact must not create a path product");
        }
    }

    #[tokio::test]
    async fn empty_text_and_video_products_create_no_index_row() {
        for (bytes, mime) in [(b"   ".to_vec(), "text/plain"), (Vec::new(), "video/mp4")] {
            let (bridge, dir, _db) = bridge().await;
            let result = bridge
                .persist(PersistAsset {
                    bytes,
                    mime: mime.into(),
                    in_library: true,
                    origin: json!({"prompt": "must fail"}),
                })
                .await;
            assert!(result.is_err());
            assert!(bridge.repo.list_all_assets().await.unwrap().is_empty());
            let assets_dir = dir.path().join("workshop").join("assets");
            assert_eq!(std::fs::read_dir(assets_dir).map(|entries| entries.count()).unwrap_or(0), 0);
        }
    }

    #[tokio::test]
    async fn persist_text_honors_in_library_false_and_title_fallback() {
        let (bridge, _dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                bytes: b"draft".to_vec(),
                mime: "text/plain; charset=utf-8".into(),
                in_library: false,
                origin: json!({}),
            })
            .await
            .unwrap();
        let row = bridge.repo.get_asset(&id).await.unwrap().unwrap();
        assert!(!row.in_library);
        assert_eq!(row.title, id, "title falls back to id when no prompt");
    }

    #[tokio::test]
    async fn load_text_asset_returns_utf8_bytes() {
        let (bridge, _dir, _db) = bridge().await;
        let id = bridge
            .persist(PersistAsset {
                bytes: "reusable prompt text".as_bytes().to_vec(),
                mime: "text/plain; charset=utf-8".into(),
                in_library: true,
                origin: json!({"prompt": "seed"}),
            })
            .await
            .unwrap();
        let loaded = bridge.load(&id).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&loaded.bytes), "reusable prompt text");
        assert!(loaded.mime.starts_with("text/plain"));
    }
}
