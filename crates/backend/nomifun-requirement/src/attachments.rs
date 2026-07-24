use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use nomifun_api_types::{AttachmentDto, NewAttachmentRef};
use nomifun_common::{
    AppError, AttachmentId, RequirementId, generate_id, now_ms,
    workspace_path_has_edge_whitespace_segment,
};
use nomifun_db::IAttachmentRepository;
use nomifun_db::models::AttachmentRow;
use nomifun_file::path_safety::{has_traversal, validate_path};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

/// Upload whitelist —images only this iteration, aligned with the frontend
/// `imageExts` (FileService.ts).
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "svg"];

/// Directory (relative to the data dir) where attachment originals live:
/// `attachments/{requirement_id}/{att_id}.{ext}`. The former generic
/// `{kind}/{target_id}` polymorphism collapsed to a single requirement domain.
const ATTACHMENTS_REL_DIR: &str = "attachments";
const DELETE_JOURNAL_FILE: &str = ".delete-journal-v1.json";
const DELETE_JOURNAL_VERSION: u32 = 1;
const MAX_DELETE_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

/// Directory (relative to a session workspace) where AutoWork stages copies
/// for the model to read.
///
/// Disk names are always `{attachment_id}.{lowercase_whitelisted_extension}`.
/// The user-controlled display name is prompt text only and never participates
/// in a filesystem path.
const WORKSPACE_STAGE_REL_DIR: &str = ".nomi/requirement-attachments";

/// An attachment entry rendered into a requirement prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptAttachment {
    /// Original display name ("设计稿.png").
    pub file_name: String,
    /// Path the model should read: workspace-relative (forward slashes) when
    /// staged into the session workspace, absolute otherwise. Empty when missing.
    pub path: String,
    /// The original file vanished from the attachment store —listed so the
    /// model knows an image existed but cannot be read.
    pub missing: bool,
}

/// Read-only attachment plan used to construct the exact AutoWork prompt
/// before any runtime, knowledge mount, or workspace mutation is activated.
///
/// The prompt-facing paths are final. Activation therefore fails closed if a
/// planned copy cannot be completed; it never silently swaps a relative path
/// for an absolute fallback after the durable request fingerprint was checked.
#[derive(Debug, Clone)]
pub(crate) struct PromptAttachmentPlan {
    pub attachments: Vec<PromptAttachment>,
    workspace: Option<PathBuf>,
    copies: Vec<PromptAttachmentCopy>,
}

impl PromptAttachmentPlan {
    pub(crate) fn empty() -> Self {
        Self {
            attachments: Vec::new(),
            workspace: None,
            copies: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct PromptAttachmentCopy {
    row: AttachmentRow,
    disk_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentDeleteJournal {
    version: u32,
    requirement_id: String,
    operation_id: String,
    entries: Vec<AttachmentDeleteJournalEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentDeleteJournalEntry {
    attachment_id: String,
    disk_name: String,
    staged_name: String,
    identity_volume: u64,
    identity_index: u64,
}

impl AttachmentDeleteJournalEntry {
    fn identity(&self) -> FileIdentity {
        FileIdentity {
            volume: self.identity_volume,
            index: self.identity_index,
        }
    }
}

type WorkspaceStageLock = AsyncMutex<()>;
type AttachmentMutationLock = AsyncMutex<()>;

/// Multiple conversations may share a custom workspace. Staging is therefore
/// serialized by physical workspace identity rather than by conversation.
/// Weak values keep this process-global registry bounded.
fn workspace_stage_lock(workspace: &Path) -> Arc<WorkspaceStageLock> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<WorkspaceStageLock>>>> =
        OnceLock::new();

    let key = workspace_stage_lock_key(workspace);
    let registry = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// Filesystem mutation and crash recovery for one Requirement must be
/// serialized across ingest/remove/delete/staging. SQLite serializes writers,
/// but it cannot serialize the filesystem work that deliberately happens on
/// either side of a database transaction.
fn attachment_mutation_lock(
    data_dir: &Path,
    requirement_id: &str,
) -> Arc<AttachmentMutationLock> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<AttachmentMutationLock>>>> =
        OnceLock::new();

    let key = workspace_stage_lock_key(data_dir)
        .join(ATTACHMENTS_REL_DIR)
        .join(requirement_id);
    let registry = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn workspace_stage_lock_key(workspace: &Path) -> PathBuf {
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(workspace))
            .unwrap_or_else(|_| workspace.to_path_buf())
    };
    let mut existing_prefix = absolute.clone();
    let mut missing_suffix = Vec::<OsString>::new();
    loop {
        match std::fs::canonicalize(&existing_prefix) {
            Ok(mut canonical) => {
                for component in missing_suffix.iter().rev() {
                    canonical.push(component);
                }
                return workspace_lock_comparison_path(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(component) = existing_prefix.file_name().map(OsStr::to_os_string) else {
                    break;
                };
                if !existing_prefix.pop() {
                    break;
                }
                missing_suffix.push(component);
            }
            Err(_) => break,
        }
    }
    workspace_lock_comparison_path(absolute)
}

#[cfg(windows)]
fn workspace_lock_comparison_path(path: PathBuf) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn workspace_lock_comparison_path(path: PathBuf) -> PathBuf {
    path
}

/// Persistent attachment storage under `<data_dir>/attachments/`.
///
/// Files are copied here from the temp upload root at bind time (create/update)
/// so they survive both OS temp cleaning and conversation deletion —
/// requirements deliberately outlive their executing sessions.
pub struct AttachmentStore {
    data_dir: PathBuf,
    /// Only files inside this root may be bound (`POST /api/fs/upload` lands
    /// here). Overridable for tests.
    upload_root: PathBuf,
    repo: Arc<dyn IAttachmentRepository>,
    #[cfg(test)]
    delete_stage_cutpoint: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl AttachmentStore {
    pub fn new(data_dir: PathBuf, repo: Arc<dyn IAttachmentRepository>) -> Self {
        Self {
            data_dir,
            upload_root: std::env::temp_dir().join("nomifun"),
            repo,
            #[cfg(test)]
            delete_stage_cutpoint: None,
        }
    }

    /// Override the allowed upload source root (tests).
    pub fn with_upload_root(mut self, root: PathBuf) -> Self {
        self.upload_root = root;
        self
    }

    fn requirement_dir(&self, requirement_id: &str) -> PathBuf {
        self.data_dir
            .join(ATTACHMENTS_REL_DIR)
            .join(requirement_id)
    }

    pub fn abs_path(&self, row: &AttachmentRow) -> PathBuf {
        match validated_attachment_disk_name(row) {
            Ok(disk_name) => self
                .data_dir
                .join(ATTACHMENTS_REL_DIR)
                .join(&row.requirement_id)
                .join(disk_name),
            Err(_) => self
                .data_dir
                .join(ATTACHMENTS_REL_DIR)
                .join(".invalid-attachment-path"),
        }
    }

    pub fn to_dto(&self, row: &AttachmentRow) -> AttachmentDto {
        AttachmentDto {
            attachment_id: row.attachment_id.clone(),
            file_name: row.file_name.clone(),
            mime: row.mime.clone(),
            size_bytes: row.size_bytes,
            created_at: row.created_at,
            abs_path: self.abs_path(row).to_string_lossy().to_string(),
        }
    }

    async fn acquire_reconciled_requirement(
        &self,
        requirement_id: &str,
    ) -> Result<
        (
            tokio::sync::OwnedMutexGuard<()>,
            Vec<AttachmentRow>,
        ),
        AppError,
    > {
        validate_requirement_id(requirement_id)?;
        let guard = attachment_mutation_lock(&self.data_dir, requirement_id)
            .lock_owned()
            .await;
        let rows = self.repo.list_for_requirement(requirement_id).await?;
        let data_dir = self.data_dir.clone();
        let requirement_id = requirement_id.to_owned();
        let recovery_rows = rows.clone();
        let (guard, recovery) = tokio::task::spawn_blocking(move || {
            // The guard is deliberately owned by the blocking task. Aborting
            // the async caller must not let another mutation race filesystem
            // recovery that is still running on the blocking pool.
            let recovery =
                reconcile_delete_journal_blocking(&data_dir, &requirement_id, &recovery_rows)
                    .and_then(|()| {
                        if recovery_rows.is_empty() {
                            remove_requirement_dir_if_empty_blocking(
                                &data_dir,
                                &requirement_id,
                            )?;
                        }
                        Ok(())
                    });
            (guard, recovery)
        })
        .await
        .map_err(|error| {
            AppError::Internal(format!(
                "attachment delete-journal recovery task failed: {error}"
            ))
        })?;
        recovery
            .map_err(|error| attachment_stage_error("recover attachment delete journal", error))?;
        Ok((guard, rows))
    }

    /// Recover every durable attachment delete left by a process crash.
    ///
    /// This is a boot gate, not best-effort housekeeping. In particular, a
    /// Requirement whose database deletion committed cannot be relied upon to
    /// be accessed again, so per-Requirement lazy recovery alone would leak its
    /// quarantined files forever. The application must await this before it
    /// accepts mutation requests.
    pub async fn recover_pending_deletes(&self) -> Result<(), AppError> {
        let data_dir = self.data_dir.clone();
        let requirement_ids =
            tokio::task::spawn_blocking(move || pending_delete_requirements_blocking(&data_dir))
                .await
                .map_err(|error| {
                    AppError::Internal(format!(
                        "attachment delete-journal boot scan task failed: {error}"
                    ))
                })?
                .map_err(|error| {
                    attachment_stage_error("scan durable attachment deletes at boot", error)
                })?;
        for requirement_id in requirement_ids {
            let (_guard, _rows) = self
                .acquire_reconciled_requirement(&requirement_id)
                .await?;
        }
        Ok(())
    }

    pub async fn list(&self, requirement_id: &str) -> Result<Vec<AttachmentRow>, AppError> {
        let (_guard, rows) = self
            .acquire_reconciled_requirement(requirement_id)
            .await?;
        Ok(rows)
    }

    /// Build the exact prompt attachment list without touching the workspace.
    ///
    /// This is the first half of AutoWork's replay-safe plan/activate split:
    /// callers can validate a complete durable message payload and absorb an
    /// existing receipt before runtime/knowledge/staging side effects begin.
    pub(crate) async fn plan_for_prompt(
        &self,
        req_id: &str,
        workspace: Option<&Path>,
    ) -> Result<PromptAttachmentPlan, AppError> {
        let (_guard, rows) = self.acquire_reconciled_requirement(req_id).await?;
        let workspace = workspace.filter(|path| !path.as_os_str().is_empty());
        let mut attachments = Vec::with_capacity(rows.len());
        let mut copies = Vec::with_capacity(rows.len());

        for row in rows {
            if row.requirement_id != req_id {
                return Err(AppError::Internal(
                    "attachment repository returned a row outside the requested requirement"
                        .to_owned(),
                ));
            }
            let disk_name = validated_attachment_disk_name(&row).map_err(|error| {
                attachment_stage_error("validate persisted attachment identity", error)
            })?;
            let source = resolve_attachment_source(&self.data_dir, &row).map_err(|error| {
                attachment_stage_error("validate persisted attachment source", error)
            })?;
            let Some(source) = source else {
                attachments.push(PromptAttachment {
                    file_name: row.file_name,
                    path: String::new(),
                    missing: true,
                });
                continue;
            };

            if workspace.is_some() {
                let file_name = row.file_name.clone();
                copies.push(PromptAttachmentCopy {
                    row,
                    disk_name: disk_name.clone(),
                });
                attachments.push(PromptAttachment {
                    file_name,
                    path: format!("./{WORKSPACE_STAGE_REL_DIR}/{req_id}/{disk_name}"),
                    missing: false,
                });
            } else {
                let source = source.to_str().ok_or_else(|| {
                    AppError::Forbidden(
                        "persisted attachment path is not valid UTF-8 and cannot be represented in a prompt"
                            .to_owned(),
                    )
                })?;
                attachments.push(PromptAttachment {
                    file_name: row.file_name,
                    path: source.to_owned(),
                    missing: false,
                });
            }
        }

        Ok(PromptAttachmentPlan {
            attachments,
            workspace: workspace.map(Path::to_path_buf),
            copies,
        })
    }

    /// Activate a previously read-only prompt plan.
    ///
    /// No fallback is allowed here: changing a prompt path after receipt
    /// preflight would change the request payload under the same idempotency
    /// key. A copy error is returned before the keyed send can claim execution.
    pub(crate) async fn activate_prompt_plan(
        &self,
        plan: &PromptAttachmentPlan,
    ) -> Result<(), AppError> {
        if plan.copies.is_empty() {
            return Ok(());
        }
        let workspace = plan.workspace.as_ref().ok_or_else(|| {
            AppError::Internal("attachment plan has copies without a workspace".to_owned())
        })?;
        let requirement_id = plan
            .copies
            .first()
            .map(|copy| copy.row.requirement_id.as_str())
            .ok_or_else(|| {
                AppError::Internal("attachment plan unexpectedly has no copies".to_owned())
            })?;
        if plan
            .copies
            .iter()
            .any(|copy| copy.row.requirement_id != requirement_id)
        {
            return Err(AppError::Internal(
                "attachment plan spans multiple requirements".to_owned(),
            ));
        }
        let (mutation_guard, current_rows) =
            self.acquire_reconciled_requirement(requirement_id).await?;
        let current_attachments: HashMap<String, String> = current_rows
            .iter()
            .map(|row| {
                validated_attachment_disk_name(row)
                    .map(|disk_name| (row.attachment_id.clone(), disk_name))
            })
            .collect::<io::Result<_>>()
            .map_err(|error| {
                attachment_stage_error("validate current attachment plan identity", error)
            })?;
        for copy in &plan.copies {
            if current_attachments.get(&copy.row.attachment_id) != Some(&copy.disk_name) {
                return Err(AppError::Conflict(
                    "attachment plan changed before workspace activation".to_owned(),
                ));
            }
        }
        let workspace = workspace.clone();
        let data_dir = self.data_dir.clone();
        let copies = plan.copies.clone();
        let stage_guard = workspace_stage_lock(&workspace).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            // Cancellation of the async caller must not release the workspace
            // or source transaction while blocking copy/publish work runs.
            let _stage_guard = stage_guard;
            let _mutation_guard = mutation_guard;
            activate_prompt_plan_blocking(&data_dir, &workspace, &copies)
        })
        .await
        .map_err(|error| {
            AppError::Internal(format!(
                "AutoWork attachment staging task failed: {error}"
            ))
        })?
        .map_err(|error| attachment_stage_error("activate AutoWork attachment plan", error))
    }

    /// Validate + copy `refs` into the persistent store and insert rows.
    /// All-or-nothing per call: any failure removes the files and rows created
    /// by THIS call before returning the error.
    pub async fn ingest(
        &self,
        requirement_id: &str,
        refs: &[NewAttachmentRef],
        created_by: Option<&str>,
    ) -> Result<Vec<AttachmentRow>, AppError> {
        validate_requirement_id(requirement_id)?;
        let (_mutation_guard, _) = self
            .acquire_reconciled_requirement(requirement_id)
            .await?;
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        // Pre-validate everything before touching disk or DB.
        let mut validated: Vec<(PathBuf, String)> = Vec::with_capacity(refs.len()); // (source, ext)
        for r in refs {
            let ext = image_ext(&r.file_name).or_else(|| image_ext(&r.source_path)).ok_or_else(|| {
                AppError::BadRequest(format!(
                    "attachment '{}' is not a supported image (allowed: {})",
                    r.file_name,
                    IMAGE_EXTENSIONS.join("/")
                ))
            })?;
            if has_traversal(&r.source_path) {
                return Err(AppError::BadRequest(format!(
                    "source path '{}' contains invalid traversal patterns",
                    r.source_path
                )));
            }
            let canonical = validate_path(&r.source_path, &[self.upload_root.as_path()])?;
            validated.push((canonical, ext));
        }

        // Per-requirement display-name dedup: existing rows + this batch.
        let mut used_names: Vec<String> = self
            .repo
            .list_for_requirement(requirement_id)
            .await?
            .into_iter()
            .map(|r| r.file_name)
            .collect();

        let dir = self.requirement_dir(requirement_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| AppError::Internal(format!("create attachment dir failed: {e}")))?;

        let mut inserted: Vec<AttachmentRow> = Vec::with_capacity(refs.len());
        let mut copied: Vec<PathBuf> = Vec::with_capacity(refs.len());
        for (r, (source, ext)) in refs.iter().zip(validated) {
            let result = self
                .ingest_one(
                    requirement_id,
                    r,
                    &source,
                    &ext,
                    created_by,
                    &mut used_names,
                    &dir,
                )
                .await;
            match result {
                Ok((row, abs)) => {
                    copied.push(abs);
                    inserted.push(row);
                }
                Err(e) => {
                    // Roll back THIS call's work: rows then files (best-effort).
                    for row in &inserted {
                        if let Err(de) = self.repo.delete(row.id).await {
                            warn!(error = %de, id = row.id, "attachment rollback: row delete failed");
                        }
                    }
                    for p in &copied {
                        if let Err(fe) = tokio::fs::remove_file(p).await {
                            warn!(error = %fe, path = %p.display(), "attachment rollback: file delete failed");
                        }
                    }
                    let _ = tokio::fs::remove_dir(&dir).await; // ok if non-empty
                    return Err(e);
                }
            }
        }
        Ok(inserted)
    }

    #[allow(clippy::too_many_arguments)]
    async fn ingest_one(
        &self,
        requirement_id: &str,
        r: &NewAttachmentRef,
        source: &Path,
        ext: &str,
        created_by: Option<&str>,
        used_names: &mut Vec<String>,
        dir: &Path,
    ) -> Result<(AttachmentRow, PathBuf), AppError> {
        let attachment_id = AttachmentId::new().into_string();
        let disk_name = format!("{attachment_id}.{ext}");
        let abs = dir.join(&disk_name);
        // A missing source is the caller's fault (stale temp ref → BadRequest);
        // anything else (disk full, permissions) is a server-side failure.
        tokio::fs::copy(source, &abs).await.map_err(|e| {
            let msg = format!("cannot copy attachment '{}': {e}", r.file_name);
            match e.kind() {
                std::io::ErrorKind::NotFound => AppError::BadRequest(msg),
                _ => AppError::Internal(msg),
            }
        })?;
        let size_bytes = match tokio::fs::metadata(&abs).await {
            Ok(m) => m.len() as i64,
            Err(e) => {
                warn!(error = %e, path = %abs.display(), "attachment metadata read failed —recording size 0");
                0
            }
        };
        let file_name = unique_name(&r.file_name, used_names);
        used_names.push(file_name.clone());
        let row = AttachmentRow {
            id: 0,
            attachment_id,
            requirement_id: requirement_id.to_owned(),
            file_name,
            rel_path: format!("{ATTACHMENTS_REL_DIR}/{requirement_id}/{disk_name}"),
            mime: mime_for_ext(ext).to_string(),
            size_bytes,
            created_by: created_by.map(|s| s.to_string()),
            created_at: now_ms(),
        };
        match self.repo.insert(&row).await {
            Ok(inserted) => Ok((inserted, abs)),
            Err(e) => {
                let _ = tokio::fs::remove_file(&abs).await;
                Err(e.into())
            }
        }
    }

    /// Remove specific attachments (rows + files). Ids that don't exist or
    /// belong to a different requirement are skipped —scope guard.
    pub async fn remove(
        &self,
        requirement_id: &str,
        attachment_ids: &[String],
    ) -> Result<(), AppError> {
        validate_requirement_id(requirement_id)?;
        for attachment_id in attachment_ids {
            AttachmentId::try_from(attachment_id.as_str())
                .map_err(|error| AppError::BadRequest(format!("invalid attachment id: {error}")))?;
        }
        let (mutation_guard, current_rows) =
            self.acquire_reconciled_requirement(requirement_id).await?;
        let requested: HashSet<&str> = attachment_ids.iter().map(String::as_str).collect();
        let rows: Vec<_> = current_rows
            .into_iter()
            .filter(|row| requested.contains(row.attachment_id.as_str()))
            .collect();
        if rows.is_empty() {
            return Ok(());
        }

        let prepared = self
            .prepare_rows_for_delete(requirement_id, &rows, mutation_guard)
            .await?;
        for row in rows {
            if let Err(error) = self.repo.delete(row.id).await {
                if let Err(recovery_error) = self.restore_prepared_delete(prepared).await {
                    return Err(AppError::Internal(format!(
                        "attachment database delete failed ({error}); durable filesystem reconciliation also failed ({recovery_error})"
                    )));
                }
                return Err(error.into());
            }
        }
        self.finish_prepared_delete(prepared).await;
        Ok(())
    }

    /// Delete every attachment of a requirement (rows + files + dir).
    ///
    /// A durable same-directory journal is synced before any source path is
    /// quarantined. Recovery then treats current database rows as authority:
    /// surviving rows restore their exact files, while absent rows clean the
    /// exact journal-owned files. This also resolves cancellation and
    /// ambiguous-commit windows without guessing whether SQLite committed.
    pub async fn delete_all(&self, requirement_id: &str) -> Result<(), AppError> {
        let prepared = self.prepare_delete_all(requirement_id).await?;
        match self.repo.delete_for_requirement(requirement_id).await {
            Ok(_) => {
                self.finish_prepared_delete(prepared).await;
                Ok(())
            }
            Err(error) => {
                self.restore_prepared_delete(prepared).await?;
                Err(error.into())
            }
        }
    }

    pub(crate) async fn prepare_delete_all(
        &self,
        requirement_id: &str,
    ) -> Result<PreparedAttachmentDelete, AppError> {
        validate_requirement_id(requirement_id)?;
        let (mutation_guard, rows) =
            self.acquire_reconciled_requirement(requirement_id).await?;
        self.prepare_rows_for_delete(requirement_id, &rows, mutation_guard)
            .await
    }

    async fn prepare_rows_for_delete(
        &self,
        requirement_id: &str,
        rows: &[AttachmentRow],
        mutation_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<PreparedAttachmentDelete, AppError> {
        let data_dir = self.data_dir.clone();
        let requirement_id = requirement_id.to_owned();
        let rows = rows.to_vec();
        let repo = Arc::clone(&self.repo);
        #[cfg(test)]
        let delete_stage_cutpoint = self.delete_stage_cutpoint.clone();
        tokio::task::spawn_blocking(move || {
            prepare_delete_journal_blocking(&data_dir, &requirement_id, &rows)?;
            #[cfg(test)]
            if let Some(cutpoint) = delete_stage_cutpoint {
                cutpoint();
            }
            Ok::<_, io::Error>(PreparedAttachmentDelete {
                recovery: Some(PreparedDeleteRecovery {
                    data_dir,
                    requirement_id,
                    repo,
                    mutation_guard,
                }),
            })
        })
        .await
        .map_err(|error| {
            AppError::Internal(format!(
                "attachment delete-journal staging task failed: {error}"
            ))
        })?
        .map_err(|error| attachment_stage_error("prepare durable attachment delete", error))
    }

    pub(crate) async fn restore_prepared_delete(
        &self,
        prepared: PreparedAttachmentDelete,
    ) -> Result<(), AppError> {
        self.reconcile_prepared_delete(prepared).await
    }

    pub(crate) async fn finish_prepared_delete(&self, prepared: PreparedAttachmentDelete) {
        if let Err(error) = self.reconcile_prepared_delete(prepared).await {
            warn!(
                error = %error,
                "attachment delete journal was left for authoritative recovery after committed delete"
            );
        }
    }

    async fn reconcile_prepared_delete(
        &self,
        mut prepared: PreparedAttachmentDelete,
    ) -> Result<(), AppError> {
        let (repo, requirement_id) = prepared
            .recovery
            .as_ref()
            .map(|recovery| (Arc::clone(&recovery.repo), recovery.requirement_id.clone()))
            .ok_or_else(|| {
                AppError::Internal("attachment delete transaction is already settled".to_owned())
            })?;
        // Keep `prepared` armed while this database read is in flight. If the
        // caller is cancelled here, Drop schedules the same authoritative
        // reconciliation instead of merely releasing the mutation lock.
        let rows = repo.list_for_requirement(&requirement_id).await?;
        let recovery = prepared.recovery.take().ok_or_else(|| {
            AppError::Internal("attachment delete transaction is already settled".to_owned())
        })?;
        run_prepared_delete_reconciliation(recovery, rows)
            .await
            .map_err(|error| {
                attachment_stage_error("reconcile durable attachment delete", error)
            })
    }

    /// Compatibility wrapper around the fail-closed plan/activate primitive.
    /// Workspace activation failures become missing prompt entries; they never
    /// change the preflighted prompt to an absolute-path fallback.
    pub async fn stage_for_prompt(
        &self,
        req_id: &str,
        workspace: Option<&Path>,
    ) -> Vec<PromptAttachment> {
        let plan = match self.plan_for_prompt(req_id, workspace).await {
            Ok(plan) => plan,
            Err(error) => {
                warn!(%error, req_id, "failed to build a safe attachment staging plan");
                return Vec::new();
            }
        };
        if let Err(error) = self.activate_prompt_plan(&plan).await {
            warn!(%error, req_id, "attachment staging activation failed closed");
            return plan
                .attachments
                .into_iter()
                .map(|mut attachment| {
                    if !attachment.missing {
                        attachment.path.clear();
                        attachment.missing = true;
                    }
                    attachment
                })
                .collect();
        }
        plan.attachments
    }
}

async fn run_prepared_delete_reconciliation(
    recovery: PreparedDeleteRecovery,
    rows: Vec<AttachmentRow>,
) -> Result<(), io::Error> {
    tokio::task::spawn_blocking(move || {
        let PreparedDeleteRecovery {
            data_dir,
            requirement_id,
            repo: _,
            mutation_guard,
        } = recovery;
        let _mutation_guard = mutation_guard;
        reconcile_delete_journal_blocking(&data_dir, &requirement_id, &rows)?;
        if rows.is_empty() {
            remove_requirement_dir_if_empty_blocking(&data_dir, &requirement_id)?;
        }
        Ok::<_, io::Error>(())
    })
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "attachment delete-journal reconciliation task failed: {error}"
        ))
    })?
}

async fn reconcile_abandoned_prepared_delete(recovery: PreparedDeleteRecovery) {
    let rows = match recovery
        .repo
        .list_for_requirement(&recovery.requirement_id)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            warn!(
                error = %error,
                requirement_id = %recovery.requirement_id,
                "cancelled attachment delete remains journaled after recovery database read failed"
            );
            return;
        }
    };
    if let Err(error) = run_prepared_delete_reconciliation(recovery, rows).await {
        warn!(
            error = %error,
            "cancelled attachment delete remains journaled after background reconciliation failed"
        );
    }
}

impl Drop for PreparedAttachmentDelete {
    fn drop(&mut self) {
        let Some(recovery) = self.recovery.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // A durable journal is already on disk. The mandatory application
            // boot sweep will reconcile it when no runtime is available here.
            return;
        };
        runtime.spawn(reconcile_abandoned_prepared_delete(recovery));
    }
}

impl PreparedAttachmentDelete {
    #[cfg(test)]
    fn simulate_process_interruption(mut self) {
        // A real process crash cannot run Drop. Tests use this to leave the
        // durable journal on disk while still releasing this process's mutex.
        drop(self.recovery.take());
    }
}

fn attachment_stage_error(context: &str, error: io::Error) -> AppError {
    let message = format!("{context} failed closed: {error}");
    match error.kind() {
        io::ErrorKind::InvalidData
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::PermissionDenied
        | io::ErrorKind::AlreadyExists
        | io::ErrorKind::Unsupported => AppError::Forbidden(message),
        _ => AppError::Internal(message),
    }
}

fn invalid_attachment_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Reconstruct the only disk name accepted for a persisted attachment.
///
/// `file_name` is deliberately absent from this function: it is untrusted
/// display text and may contain separators, roots, reserved names, or aliases.
fn validated_attachment_disk_name(row: &AttachmentRow) -> io::Result<String> {
    AttachmentId::parse(row.attachment_id.clone()).map_err(|error| {
        invalid_attachment_data(format!(
            "attachment row has a non-canonical attachment_id: {error}"
        ))
    })?;
    RequirementId::parse(row.requirement_id.clone()).map_err(|error| {
        invalid_attachment_data(format!(
            "attachment row has a non-canonical requirement_id: {error}"
        ))
    })?;

    let mut components = Path::new(&row.rel_path).components();
    let (Some(Component::Normal(root)), Some(Component::Normal(requirement_id)), Some(Component::Normal(file_name))) =
        (components.next(), components.next(), components.next())
    else {
        return Err(invalid_attachment_data(
            "attachment rel_path is not the canonical three-component relative path",
        ));
    };
    if components.next().is_some()
        || root != OsStr::new(ATTACHMENTS_REL_DIR)
        || requirement_id != OsStr::new(&row.requirement_id)
    {
        return Err(invalid_attachment_data(
            "attachment rel_path does not match its persisted requirement identity",
        ));
    }
    let file_name = file_name.to_str().ok_or_else(|| {
        invalid_attachment_data("attachment rel_path disk name is not valid UTF-8")
    })?;
    let expected =
        validated_identity_disk_name(&row.attachment_id, file_name)?;
    let expected_rel_path = format!(
        "{ATTACHMENTS_REL_DIR}/{}/{}",
        row.requirement_id, expected
    );
    if row.rel_path != expected_rel_path {
        return Err(invalid_attachment_data(
            "attachment rel_path is not the exact portable forward-slash representation",
        ));
    }
    Ok(expected)
}

fn validated_identity_disk_name(
    attachment_id: &str,
    file_name: &str,
) -> io::Result<String> {
    AttachmentId::parse(attachment_id.to_owned()).map_err(|error| {
        invalid_attachment_data(format!(
            "attachment disk name has a non-canonical attachment identity: {error}"
        ))
    })?;
    let prefix = format!("{attachment_id}.");
    let extension = file_name.strip_prefix(&prefix).ok_or_else(|| {
        invalid_attachment_data(
            "attachment rel_path disk name does not begin with its attachment identity",
        )
    })?;
    if extension.is_empty()
        || extension != extension.to_ascii_lowercase()
        || !extension.bytes().all(|byte| byte.is_ascii_lowercase())
        || !IMAGE_EXTENSIONS.contains(&extension)
    {
        return Err(invalid_attachment_data(
            "attachment rel_path extension is not a lowercase whitelisted image extension",
        ));
    }
    let expected = format!("{attachment_id}.{extension}");
    if file_name != expected {
        return Err(invalid_attachment_data(
            "attachment disk name is not canonical",
        ));
    }
    Ok(expected)
}

#[derive(Clone, Copy)]
enum PlainPathKind {
    Directory,
    File,
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_plain_metadata(
    metadata: &std::fs::Metadata,
    kind: PlainPathKind,
    context: &str,
) -> io::Result<()> {
    validate_plain_type_metadata(metadata, kind, context)?;
    if matches!(kind, PlainPathKind::File) && !metadata_has_one_link(metadata) {
        return Err(invalid_attachment_data(format!(
            "{context} is hard-linked or its link count cannot be verified"
        )));
    }
    Ok(())
}

fn validate_plain_type_metadata(
    metadata: &std::fs::Metadata,
    kind: PlainPathKind,
    context: &str,
) -> io::Result<()> {
    if metadata_is_reparse(metadata) {
        return Err(invalid_attachment_data(format!(
            "{context} is a symbolic link, junction, or reparse point"
        )));
    }
    let has_expected_type = match kind {
        PlainPathKind::Directory => metadata.is_dir(),
        PlainPathKind::File => metadata.is_file(),
    };
    if !has_expected_type {
        return Err(invalid_attachment_data(format!(
            "{context} is not the required ordinary filesystem object"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_has_one_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() == 1
}

#[cfg(windows)]
fn metadata_has_one_link(_metadata: &std::fs::Metadata) -> bool {
    // Stable std metadata does not expose the Windows hard-link count. Every
    // file path is additionally opened through `open_stable_regular_file`,
    // which verifies this from GetFileInformationByHandle.
    true
}

#[cfg(not(any(unix, windows)))]
fn metadata_has_one_link(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    index: u64,
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        index: metadata.ino(),
    })
}

#[cfg(unix)]
fn file_link_count(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
#[derive(Clone, Copy)]
struct WindowsFileTime {
    dwLowDateTime: u32,
    dwHighDateTime: u32,
}

#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
#[derive(Clone, Copy)]
struct WindowsByHandleFileInformation {
    dwFileAttributes: u32,
    ftCreationTime: WindowsFileTime,
    ftLastAccessTime: WindowsFileTime,
    ftLastWriteTime: WindowsFileTime,
    dwVolumeSerialNumber: u32,
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    nNumberOfLinks: u32,
    nFileIndexHigh: u32,
    nFileIndexLow: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        information: *mut WindowsByHandleFileInformation,
    ) -> i32;
}

#[cfg(windows)]
fn windows_file_information(file: &File) -> io::Result<WindowsByHandleFileInformation> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    let mut information = MaybeUninit::<WindowsByHandleFileInformation>::zeroed();
    // SAFETY: the raw handle is borrowed from a live `File`, the output points
    // to correctly sized writable storage, and initialization is checked by
    // the Win32 return value before `assume_init`.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetFileInformationByHandle returned success and initialized the
    // complete BY_HANDLE_FILE_INFORMATION-compatible structure.
    Ok(unsafe { information.assume_init() })
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let information = windows_file_information(file)?;
    Ok(FileIdentity {
        volume: u64::from(information.dwVolumeSerialNumber),
        index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

#[cfg(windows)]
fn file_link_count(file: &File) -> io::Result<u64> {
    Ok(u64::from(
        windows_file_information(file)?.nNumberOfLinks,
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> io::Result<FileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stable file identity is unavailable on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_link_count(_file: &File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file link count is unavailable on this platform",
    ))
}

fn file_has_one_link(file: &File) -> io::Result<bool> {
    Ok(file_link_count(file)? == 1)
}

fn paths_equal_for_platform(left: &Path, right: &Path) -> bool {
    // Both operands are physical paths returned by `canonicalize` from the
    // same walked tree. Exact equality is intentional even on macOS, where an
    // APFS volume may be case-sensitive. Windows canonicalization normalizes
    // the on-disk component casing for both operands.
    left == right
}

fn reject_ascii_case_alias(parent: &Path, expected: &OsStr) -> io::Result<()> {
    let Some(expected_text) = expected.to_str() else {
        return Err(invalid_attachment_data(
            "a trusted attachment path component is not valid UTF-8",
        ));
    };
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let actual = entry.file_name();
        if actual != expected {
            if let Some(actual_text) = actual.to_str()
                && actual_text.eq_ignore_ascii_case(expected_text)
            {
                return Err(invalid_attachment_data(format!(
                    "case-colliding filesystem entry '{}' aliases expected entry '{}'",
                    actual_text, expected_text
                )));
            }
        }
    }
    Ok(())
}

fn existing_plain_child(
    parent: &Path,
    name: &OsStr,
    kind: PlainPathKind,
    context: &str,
) -> io::Result<Option<PathBuf>> {
    reject_ascii_case_alias(parent, name)?;
    let child = parent.join(name);
    let metadata = match std::fs::symlink_metadata(&child) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_plain_metadata(&metadata, kind, context)?;
    let canonical = std::fs::canonicalize(&child)?;
    let canonical_parent = std::fs::canonicalize(parent)?;
    let actual_parent = canonical.parent().ok_or_else(|| {
        invalid_attachment_data(format!("{context} has no physical parent directory"))
    })?;
    if !paths_equal_for_platform(actual_parent, &canonical_parent) {
        return Err(invalid_attachment_data(format!(
            "{context} resolves outside its expected physical parent"
        )));
    }
    Ok(Some(canonical))
}

fn ensure_plain_directory(parent: &Path, name: &OsStr, context: &str) -> io::Result<PathBuf> {
    if let Some(path) =
        existing_plain_child(parent, name, PlainPathKind::Directory, context)?
    {
        return Ok(path);
    }
    let child = parent.join(name);
    match std::fs::create_dir(&child) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    existing_plain_child(parent, name, PlainPathKind::Directory, context)?.ok_or_else(|| {
        invalid_attachment_data(format!("{context} disappeared while it was being created"))
    })
}

fn resolve_attachment_requirement_root(
    data_dir: &Path,
    requirement_id: &str,
) -> io::Result<Option<PathBuf>> {
    RequirementId::parse(requirement_id.to_owned()).map_err(|error| {
        invalid_attachment_data(format!(
            "attachment requirement directory has a non-canonical identity: {error}"
        ))
    })?;
    let data_root = match std::fs::canonicalize(data_dir) {
        Ok(data_root) => data_root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_plain_metadata(
        &std::fs::symlink_metadata(&data_root)?,
        PlainPathKind::Directory,
        "attachment data root",
    )?;
    let Some(attachments_root) = existing_plain_child(
        &data_root,
        OsStr::new(ATTACHMENTS_REL_DIR),
        PlainPathKind::Directory,
        "attachment storage root",
    )?
    else {
        return Ok(None);
    };
    let Some(requirement_root) = existing_plain_child(
        &attachments_root,
        OsStr::new(requirement_id),
        PlainPathKind::Directory,
        "attachment requirement directory",
    )?
    else {
        return Ok(None);
    };
    Ok(Some(requirement_root))
}

fn pending_delete_requirements_blocking(data_dir: &Path) -> io::Result<Vec<String>> {
    let data_root = match std::fs::canonicalize(data_dir) {
        Ok(data_root) => data_root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    validate_plain_metadata(
        &std::fs::symlink_metadata(&data_root)?,
        PlainPathKind::Directory,
        "attachment data root during boot recovery",
    )?;
    let Some(attachments_root) = existing_plain_child(
        &data_root,
        OsStr::new(ATTACHMENTS_REL_DIR),
        PlainPathKind::Directory,
        "attachment storage root during boot recovery",
    )?
    else {
        return Ok(Vec::new());
    };

    let mut requirement_ids = Vec::new();
    for entry in std::fs::read_dir(&attachments_root)? {
        let entry = entry?;
        let requirement_id = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                invalid_attachment_data(
                    "attachment storage root contains a non-UTF-8 Requirement directory",
                )
            })?;
        RequirementId::parse(requirement_id.clone()).map_err(|error| {
            invalid_attachment_data(format!(
                "attachment storage root contains a non-canonical Requirement directory '{requirement_id}': {error}"
            ))
        })?;
        let requirement_root = existing_plain_child(
            &attachments_root,
            OsStr::new(&requirement_id),
            PlainPathKind::Directory,
            "attachment Requirement directory during boot recovery",
        )?
        .ok_or_else(|| {
            invalid_attachment_data(
                "attachment Requirement directory disappeared during boot recovery",
            )
        })?;
        let mut needs_recovery = false;
        for child in std::fs::read_dir(&requirement_root)? {
            let child = child?;
            let Some(name) = child.file_name().to_str().map(str::to_owned) else {
                return Err(invalid_attachment_data(
                    "attachment storage contains a non-UTF-8 entry",
                ));
            };
            let folded = name.to_ascii_lowercase();
            if folded == DELETE_JOURNAL_FILE
                || folded.contains(".deleting-")
                || delete_journal_temp_operation_id(&folded).is_some()
            {
                needs_recovery = true;
                break;
            }
        }
        if needs_recovery {
            requirement_ids.push(requirement_id);
        }
    }
    requirement_ids.sort();
    Ok(requirement_ids)
}

fn remove_requirement_dir_if_empty_blocking(
    data_dir: &Path,
    requirement_id: &str,
) -> io::Result<()> {
    let Some(requirement_root) =
        resolve_attachment_requirement_root(data_dir, requirement_id)?
    else {
        return Ok(());
    };
    match std::fs::remove_dir(requirement_root) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn resolve_attachment_source(
    data_dir: &Path,
    row: &AttachmentRow,
) -> io::Result<Option<PathBuf>> {
    let disk_name = validated_attachment_disk_name(row)?;
    let Some(requirement_root) =
        resolve_attachment_requirement_root(data_dir, &row.requirement_id)?
    else {
        return Ok(None);
    };
    let Some(source) = existing_plain_child(
        &requirement_root,
        OsStr::new(&disk_name),
        PlainPathKind::File,
        "persisted attachment source",
    )?
    else {
        return Ok(None);
    };
    // Windows stable path metadata does not expose the hard-link count; opening
    // here also closes the plan-time swap/reparse window on every platform.
    drop(open_stable_regular_file(
        &source,
        "persisted attachment source",
    )?);
    Ok(Some(source))
}

#[cfg(windows)]
fn open_read_no_reparse(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(windows))]
fn open_read_no_reparse(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn open_stable_regular_file(path: &Path, context: &str) -> io::Result<File> {
    let file = open_stable_regular_file_allow_hardlinks(path, context)?;
    if !file_has_one_link(&file)? {
        return Err(invalid_attachment_data(format!(
            "{context} is hard-linked or its link count cannot be verified"
        )));
    }
    Ok(file)
}

fn open_stable_regular_file_allow_hardlinks(path: &Path, context: &str) -> io::Result<File> {
    let before = std::fs::symlink_metadata(path)?;
    validate_plain_type_metadata(&before, PlainPathKind::File, context)?;
    let file = open_read_no_reparse(path)?;
    let handle = file.metadata()?;
    validate_plain_type_metadata(&handle, PlainPathKind::File, context)?;
    let identity = file_identity(&file)?;
    let after = std::fs::symlink_metadata(path)?;
    validate_plain_type_metadata(&after, PlainPathKind::File, context)?;
    let path_handle = open_read_no_reparse(path)?;
    validate_plain_type_metadata(&path_handle.metadata()?, PlainPathKind::File, context)?;
    if file_identity(&path_handle)? != identity {
        return Err(invalid_attachment_data(format!(
            "{context} changed after it was opened"
        )));
    }
    Ok(file)
}

fn files_equal(left: &mut File, right: &mut File) -> io::Result<bool> {
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    left.seek(SeekFrom::Start(0))?;
    right.seek(SeekFrom::Start(0))?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

struct TemporaryStageFile {
    path: PathBuf,
    identity: FileIdentity,
    armed: bool,
}

impl TemporaryStageFile {
    fn verify_path_identity(&self) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(&self.path)?;
        if metadata_is_reparse(&metadata) || !metadata.is_file() {
            return Err(invalid_attachment_data(
                "attachment staging path changed into a link or non-file",
            ));
        }
        let file = open_read_no_reparse(&self.path)?;
        if file_identity(&file)? != self.identity {
            return Err(invalid_attachment_data(
                "attachment staging path changed identity",
            ));
        }
        Ok(())
    }

    fn remove_if_owned(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        match self.verify_path_identity() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.armed = false;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        std::fs::remove_file(&self.path)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for TemporaryStageFile {
    fn drop(&mut self) {
        // Never unlink a path merely because it has our random name: another
        // actor could have swapped that name after cancellation or failure.
        let _ = self.remove_if_owned();
    }
}

fn create_same_directory_temp(
    destination_dir: &Path,
    destination_name: &str,
) -> io::Result<(File, TemporaryStageFile)> {
    for _ in 0..32 {
        let temp_name = format!(".{destination_name}.staging-{}", generate_id());
        let path = destination_dir.join(temp_name);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                let identity = match file_identity(&file) {
                    Ok(identity) => identity,
                    Err(error) => {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        return Err(error);
                    }
                };
                return Ok((
                    file,
                    TemporaryStageFile {
                        path,
                        identity,
                        armed: true,
                    },
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a collision-free attachment staging file",
    ))
}

fn verify_existing_destination(
    destination: &Path,
    expected: &mut File,
) -> io::Result<()> {
    let mut existing = open_stable_regular_file(destination, "existing attachment destination")?;
    if !files_equal(&mut existing, expected)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "attachment destination already exists with different content",
        ));
    }
    Ok(())
}

fn publish_temp_no_clobber(
    destination_dir: &Path,
    destination_name: &str,
    expected: &mut File,
    temp: &mut TemporaryStageFile,
) -> io::Result<()> {
    reject_ascii_case_alias(destination_dir, OsStr::new(destination_name))?;
    let destination = destination_dir.join(destination_name);
    if file_identity(expected)? != temp.identity || !file_has_one_link(expected)? {
        return Err(invalid_attachment_data(
            "attachment staging handle changed identity or gained a hard link",
        ));
    }
    temp.verify_path_identity()?;

    match std::fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_existing_destination(&destination, expected)?;
            temp.remove_if_owned()?;
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    temp.verify_path_identity()?;
    match std::fs::hard_link(&temp.path, &destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            verify_existing_destination(&destination, expected)?;
            temp.remove_if_owned()?;
            return Ok(());
        }
        Err(error) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "filesystem cannot atomically publish a no-clobber staged attachment: {error}"
                ),
            ));
        }
    }

    temp.remove_if_owned()?;
    let published = open_stable_regular_file(&destination, "published attachment destination")?;
    if file_identity(expected)? != file_identity(&published)? || !file_has_one_link(&published)? {
        return Err(invalid_attachment_data(
            "published attachment identity or hard-link count changed",
        ));
    }
    Ok(())
}

fn publish_attachment_copy(
    source: &Path,
    destination_dir: &Path,
    destination_name: &str,
) -> io::Result<()> {
    let mut source = open_stable_regular_file(source, "persisted attachment source")?;
    let source_identity = file_identity(&source)?;
    let source_length = source.metadata()?.len();
    let (mut temp_file, mut temp_guard) =
        create_same_directory_temp(destination_dir, destination_name)?;
    io::copy(&mut source, &mut temp_file)?;
    temp_file.flush()?;
    if !files_equal(&mut source, &mut temp_file)? {
        return Err(invalid_attachment_data(
            "persisted attachment source changed while it was copied",
        ));
    }
    temp_file.sync_all()?;
    let temp_metadata = temp_file.metadata()?;
    validate_plain_metadata(
        &temp_metadata,
        PlainPathKind::File,
        "attachment staging file",
    )?;
    if file_identity(&source)? != source_identity || source.metadata()?.len() != source_length {
        return Err(invalid_attachment_data(
            "persisted attachment source changed while it was copied",
        ));
    }
    publish_temp_no_clobber(
        destination_dir,
        destination_name,
        &mut temp_file,
        &mut temp_guard,
    )
}

fn publish_static_file(
    destination_dir: &Path,
    destination_name: &str,
    contents: &[u8],
) -> io::Result<()> {
    let (mut temp_file, mut temp_guard) =
        create_same_directory_temp(destination_dir, destination_name)?;
    temp_file.write_all(contents)?;
    temp_file.flush()?;
    temp_file.sync_all()?;
    publish_temp_no_clobber(
        destination_dir,
        destination_name,
        &mut temp_file,
        &mut temp_guard,
    )
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        // FlushFileBuffers, which backs `File::sync_all` on Windows, requires
        // a handle opened for write access even when that handle names a
        // directory.
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable directory synchronization is unavailable on this platform",
    ))
}

fn expected_delete_staged_name(disk_name: &str, operation_id: &str) -> String {
    format!(".{disk_name}.deleting-{operation_id}")
}

fn delete_journal_temp_operation_id(name: &str) -> Option<&str> {
    name.strip_prefix(&format!(".{DELETE_JOURNAL_FILE}.staging-"))
}

fn reconcile_delete_journal_publish_artifacts(
    requirement_root: &Path,
) -> io::Result<()> {
    let mut temp_names = Vec::new();
    for entry in std::fs::read_dir(requirement_root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(invalid_attachment_data(
                "attachment storage contains a non-UTF-8 entry",
            ));
        };
        let folded_name = name.to_ascii_lowercase();
        let Some(operation_id) = delete_journal_temp_operation_id(&folded_name) else {
            continue;
        };
        AttachmentId::parse(operation_id.to_owned()).map_err(|error| {
            invalid_attachment_data(format!(
                "attachment delete journal has a non-canonical publish artifact: {error}"
            ))
        })?;
        let canonical_name = format!(".{DELETE_JOURNAL_FILE}.staging-{operation_id}");
        if name != canonical_name {
            return Err(invalid_attachment_data(format!(
                "attachment delete journal publish artifact '{name}' is not the exact portable lowercase representation"
            )));
        }
        temp_names.push(name);
    }
    if temp_names.is_empty() {
        return Ok(());
    }
    if temp_names.len() != 1 {
        return Err(invalid_attachment_data(
            "attachment delete journal has multiple unfinished publish artifacts",
        ));
    }

    let temp_name = &temp_names[0];
    let temp_path = requirement_root.join(temp_name);
    let temp =
        open_stable_regular_file_allow_hardlinks(&temp_path, "attachment delete journal publish artifact")?;
    let temp_identity = file_identity(&temp)?;
    let journal_path = requirement_root.join(DELETE_JOURNAL_FILE);
    match std::fs::symlink_metadata(&journal_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if file_link_count(&temp)? != 1 {
                return Err(invalid_attachment_data(
                    "unpublished attachment delete journal artifact has an unexpected hard-link count",
                ));
            }
            drop(temp);
            remove_owned_delete_path(
                requirement_root,
                temp_name,
                temp_identity,
                1,
                "unpublished attachment delete journal artifact",
            )?;
        }
        Ok(metadata) => {
            validate_plain_type_metadata(
                &metadata,
                PlainPathKind::File,
                "attachment delete journal during publish recovery",
            )?;
            let journal = open_stable_regular_file_allow_hardlinks(
                &journal_path,
                "attachment delete journal during publish recovery",
            )?;
            if file_identity(&journal)? != temp_identity
                || file_link_count(&temp)? != 2
                || file_link_count(&journal)? != 2
            {
                return Err(invalid_attachment_data(
                    "attachment delete journal publish pair does not have one exact shared identity",
                ));
            }
            drop(temp);
            drop(journal);
            remove_owned_delete_path(
                requirement_root,
                temp_name,
                temp_identity,
                2,
                "linked attachment delete journal publish artifact",
            )?;
        }
        Err(error) => return Err(error),
    }
    sync_directory(requirement_root)
}

fn validate_delete_journal(
    journal: &AttachmentDeleteJournal,
    requirement_id: &str,
    rows: &[AttachmentRow],
) -> io::Result<HashSet<String>> {
    if journal.version != DELETE_JOURNAL_VERSION
        || journal.requirement_id != requirement_id
    {
        return Err(invalid_attachment_data(
            "attachment delete journal version or Requirement identity does not match",
        ));
    }
    AttachmentId::parse(journal.operation_id.clone()).map_err(|error| {
        invalid_attachment_data(format!(
            "attachment delete journal has a non-canonical operation identity: {error}"
        ))
    })?;
    if journal.entries.is_empty() {
        return Err(invalid_attachment_data(
            "attachment delete journal has no entries",
        ));
    }

    let mut persisted_by_attachment = HashMap::new();
    for row in rows {
        if row.requirement_id != requirement_id {
            return Err(invalid_attachment_data(
                "attachment repository returned a row outside delete-journal Requirement",
            ));
        }
        let disk_name = validated_attachment_disk_name(row)?;
        if persisted_by_attachment
            .insert(row.attachment_id.clone(), disk_name)
            .is_some()
        {
            return Err(invalid_attachment_data(
                "attachment repository returned duplicate attachment identities",
            ));
        }
    }

    let mut seen_attachments = HashSet::new();
    let mut seen_disk_names = HashSet::new();
    let mut seen_staged_names = HashSet::new();
    let mut persisted_entries = HashSet::new();
    for entry in &journal.entries {
        let disk_name =
            validated_identity_disk_name(&entry.attachment_id, &entry.disk_name)?;
        if disk_name != entry.disk_name
            || entry.staged_name
                != expected_delete_staged_name(&entry.disk_name, &journal.operation_id)
        {
            return Err(invalid_attachment_data(
                "attachment delete journal contains a non-canonical staged path",
            ));
        }
        if !seen_attachments.insert(entry.attachment_id.clone())
            || !seen_disk_names.insert(entry.disk_name.clone())
            || !seen_staged_names.insert(entry.staged_name.clone())
        {
            return Err(invalid_attachment_data(
                "attachment delete journal contains duplicate entries",
            ));
        }
        if let Some(persisted_disk_name) =
            persisted_by_attachment.get(&entry.attachment_id)
        {
            if persisted_disk_name != &entry.disk_name {
                return Err(invalid_attachment_data(
                    "attachment delete journal conflicts with the persisted attachment identity",
                ));
            }
            persisted_entries.insert(entry.attachment_id.clone());
        }
    }
    Ok(persisted_entries)
}

fn read_delete_journal(
    requirement_root: &Path,
    requirement_id: &str,
    rows: &[AttachmentRow],
) -> io::Result<Option<(AttachmentDeleteJournal, HashSet<String>)>> {
    let Some(journal_path) = existing_plain_child(
        requirement_root,
        OsStr::new(DELETE_JOURNAL_FILE),
        PlainPathKind::File,
        "attachment delete journal",
    )?
    else {
        return Ok(None);
    };
    let mut file =
        open_stable_regular_file(&journal_path, "attachment delete journal")?;
    if file.metadata()?.len() > MAX_DELETE_JOURNAL_BYTES {
        return Err(invalid_attachment_data(
            "attachment delete journal exceeds its maximum size",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let journal = serde_json::from_slice::<AttachmentDeleteJournal>(&bytes)
        .map_err(|error| {
            invalid_attachment_data(format!(
                "attachment delete journal is not valid canonical JSON: {error}"
            ))
        })?;
    let persisted_entries =
        validate_delete_journal(&journal, requirement_id, rows)?;
    Ok(Some((journal, persisted_entries)))
}

fn open_delete_entry_path(
    requirement_root: &Path,
    name: &str,
    expected_identity: FileIdentity,
    context: &str,
) -> io::Result<Option<File>> {
    reject_ascii_case_alias(requirement_root, OsStr::new(name))?;
    let path = requirement_root.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            validate_plain_type_metadata(
                &metadata,
                PlainPathKind::File,
                context,
            )?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    let file = open_stable_regular_file_allow_hardlinks(&path, context)?;
    if file_identity(&file)? != expected_identity {
        return Err(invalid_attachment_data(format!(
            "{context} does not have the identity recorded by its durable journal"
        )));
    }
    Ok(Some(file))
}

fn remove_owned_delete_path(
    requirement_root: &Path,
    name: &str,
    expected_identity: FileIdentity,
    expected_links: u64,
    context: &str,
) -> io::Result<()> {
    let Some(file) = open_delete_entry_path(
        requirement_root,
        name,
        expected_identity,
        context,
    )?
    else {
        return Ok(());
    };
    if file_link_count(&file)? != expected_links {
        return Err(invalid_attachment_data(format!(
            "{context} has an unexpected hard-link count"
        )));
    }
    drop(file);
    std::fs::remove_file(requirement_root.join(name))
}

fn reconcile_delete_journal_entry(
    requirement_root: &Path,
    entry: &AttachmentDeleteJournalEntry,
    row_still_exists: bool,
) -> io::Result<()> {
    let identity = entry.identity();
    let original = open_delete_entry_path(
        requirement_root,
        &entry.disk_name,
        identity,
        "attachment delete-journal original",
    )?;
    let staged = open_delete_entry_path(
        requirement_root,
        &entry.staged_name,
        identity,
        "attachment delete-journal staged file",
    )?;

    match (original, staged, row_still_exists) {
        (Some(original), Some(staged), true) => {
            if file_link_count(&original)? != 2
                || file_link_count(&staged)? != 2
            {
                return Err(invalid_attachment_data(
                    "attachment delete rollback found an unexpected linked-file state",
                ));
            }
            drop(original);
            drop(staged);
            remove_owned_delete_path(
                requirement_root,
                &entry.staged_name,
                identity,
                2,
                "attachment delete rollback staged file",
            )?;
            let restored = open_stable_regular_file(
                &requirement_root.join(&entry.disk_name),
                "restored attachment source",
            )?;
            if file_identity(&restored)? != identity {
                return Err(invalid_attachment_data(
                    "restored attachment source changed identity",
                ));
            }
        }
        (Some(original), Some(staged), false) => {
            if file_link_count(&original)? != 2
                || file_link_count(&staged)? != 2
            {
                return Err(invalid_attachment_data(
                    "committed attachment delete found an unexpected linked-file state",
                ));
            }
            drop(original);
            drop(staged);
            remove_owned_delete_path(
                requirement_root,
                &entry.staged_name,
                identity,
                2,
                "committed attachment staged file",
            )?;
            remove_owned_delete_path(
                requirement_root,
                &entry.disk_name,
                identity,
                1,
                "committed attachment original",
            )?;
        }
        (Some(original), None, true) => {
            if file_link_count(&original)? != 1 {
                return Err(invalid_attachment_data(
                    "attachment delete rollback original has an unexpected hard-link count",
                ));
            }
        }
        (Some(original), None, false) => {
            if file_link_count(&original)? != 1 {
                return Err(invalid_attachment_data(
                    "committed attachment original has an unexpected hard-link count",
                ));
            }
            drop(original);
            remove_owned_delete_path(
                requirement_root,
                &entry.disk_name,
                identity,
                1,
                "committed attachment original",
            )?;
        }
        (None, Some(staged), true) => {
            if file_link_count(&staged)? != 1 {
                return Err(invalid_attachment_data(
                    "attachment delete rollback staged file has an unexpected hard-link count",
                ));
            }
            drop(staged);
            let original_path = requirement_root.join(&entry.disk_name);
            match std::fs::hard_link(
                requirement_root.join(&entry.staged_name),
                &original_path,
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(invalid_attachment_data(
                        "attachment delete rollback refuses to overwrite an existing original",
                    ));
                }
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "filesystem cannot atomically restore a staged attachment: {error}"
                        ),
                    ));
                }
            }
            let restored = open_delete_entry_path(
                requirement_root,
                &entry.disk_name,
                identity,
                "restored attachment original",
            )?
            .ok_or_else(|| {
                invalid_attachment_data(
                    "restored attachment original disappeared",
                )
            })?;
            let linked_staged = open_delete_entry_path(
                requirement_root,
                &entry.staged_name,
                identity,
                "linked attachment staged file",
            )?
            .ok_or_else(|| {
                invalid_attachment_data(
                    "attachment staged file disappeared during restore",
                )
            })?;
            if file_link_count(&restored)? != 2
                || file_link_count(&linked_staged)? != 2
            {
                return Err(invalid_attachment_data(
                    "attachment delete rollback could not prove its linked restore",
                ));
            }
            drop(restored);
            drop(linked_staged);
            remove_owned_delete_path(
                requirement_root,
                &entry.staged_name,
                identity,
                2,
                "attachment delete rollback staged file",
            )?;
            let restored = open_stable_regular_file(
                &original_path,
                "restored attachment source",
            )?;
            if file_identity(&restored)? != identity {
                return Err(invalid_attachment_data(
                    "restored attachment source changed identity",
                ));
            }
        }
        (None, Some(staged), false) => {
            if file_link_count(&staged)? != 1 {
                return Err(invalid_attachment_data(
                    "committed attachment staged file has an unexpected hard-link count",
                ));
            }
            drop(staged);
            remove_owned_delete_path(
                requirement_root,
                &entry.staged_name,
                identity,
                1,
                "committed attachment staged file",
            )?;
        }
        (None, None, true) => {
            return Err(invalid_attachment_data(
                "attachment row survived deletion but neither original nor journaled file exists",
            ));
        }
        (None, None, false) => {}
    }
    Ok(())
}

fn reject_unjournaled_delete_files(requirement_root: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(requirement_root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned)
        else {
            return Err(invalid_attachment_data(
                "attachment storage contains a non-UTF-8 entry",
            ));
        };
        if name.to_ascii_lowercase().contains(".deleting-") {
            return Err(invalid_attachment_data(format!(
                "attachment storage contains unjournaled delete quarantine '{name}'"
            )));
        }
    }
    Ok(())
}

fn reconcile_delete_journal_blocking(
    data_dir: &Path,
    requirement_id: &str,
    rows: &[AttachmentRow],
) -> io::Result<()> {
    let Some(requirement_root) =
        resolve_attachment_requirement_root(data_dir, requirement_id)?
    else {
        return Ok(());
    };
    reconcile_delete_journal_publish_artifacts(&requirement_root)?;
    let Some((journal, persisted_entries)) =
        read_delete_journal(&requirement_root, requirement_id, rows)?
    else {
        reject_unjournaled_delete_files(&requirement_root)?;
        return Ok(());
    };

    for entry in &journal.entries {
        reconcile_delete_journal_entry(
            &requirement_root,
            entry,
            persisted_entries.contains(&entry.attachment_id),
        )?;
    }
    sync_directory(&requirement_root)?;
    let journal_path = requirement_root.join(DELETE_JOURNAL_FILE);
    drop(open_stable_regular_file(
        &journal_path,
        "attachment delete journal before completion",
    )?);
    std::fs::remove_file(&journal_path)?;
    sync_directory(&requirement_root)?;
    reject_unjournaled_delete_files(&requirement_root)
}

fn stage_delete_journal_entry(
    requirement_root: &Path,
    entry: &AttachmentDeleteJournalEntry,
) -> io::Result<()> {
    let identity = entry.identity();
    let original = open_delete_entry_path(
        requirement_root,
        &entry.disk_name,
        identity,
        "attachment source before delete staging",
    )?
    .ok_or_else(|| {
        invalid_attachment_data(
            "attachment source disappeared before delete staging",
        )
    })?;
    if file_link_count(&original)? != 1 {
        return Err(invalid_attachment_data(
            "attachment source gained a hard link before delete staging",
        ));
    }
    drop(original);
    reject_ascii_case_alias(
        requirement_root,
        OsStr::new(&entry.staged_name),
    )?;
    match std::fs::symlink_metadata(
        requirement_root.join(&entry.staged_name),
    ) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "attachment delete quarantine already exists",
            ));
        }
        Err(error) => return Err(error),
    }

    std::fs::hard_link(
        requirement_root.join(&entry.disk_name),
        requirement_root.join(&entry.staged_name),
    )
    .map_err(|error| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "filesystem cannot atomically stage an attachment delete: {error}"
            ),
        )
    })?;
    let linked_original = open_delete_entry_path(
        requirement_root,
        &entry.disk_name,
        identity,
        "linked attachment original",
    )?
    .ok_or_else(|| {
        invalid_attachment_data(
            "attachment original disappeared while its quarantine was linked",
        )
    })?;
    let linked_staged = open_delete_entry_path(
        requirement_root,
        &entry.staged_name,
        identity,
        "linked attachment delete quarantine",
    )?
    .ok_or_else(|| {
        invalid_attachment_data(
            "attachment delete quarantine disappeared after linking",
        )
    })?;
    if file_link_count(&linked_original)? != 2
        || file_link_count(&linked_staged)? != 2
    {
        return Err(invalid_attachment_data(
            "attachment delete staging could not prove its exact linked pair",
        ));
    }
    drop(linked_original);
    drop(linked_staged);
    remove_owned_delete_path(
        requirement_root,
        &entry.disk_name,
        identity,
        2,
        "attachment original during delete staging",
    )?;
    let staged = open_stable_regular_file(
        &requirement_root.join(&entry.staged_name),
        "staged attachment delete quarantine",
    )?;
    if file_identity(&staged)? != identity {
        return Err(invalid_attachment_data(
            "staged attachment delete quarantine changed identity",
        ));
    }
    sync_directory(requirement_root)
}

fn prepare_delete_journal_blocking(
    data_dir: &Path,
    requirement_id: &str,
    rows: &[AttachmentRow],
) -> io::Result<()> {
    let Some(requirement_root) =
        resolve_attachment_requirement_root(data_dir, requirement_id)?
    else {
        return Ok(());
    };
    reject_unjournaled_delete_files(&requirement_root)?;

    let operation_id = AttachmentId::new().into_string();
    let mut entries = Vec::new();
    for row in rows {
        if row.requirement_id != requirement_id {
            return Err(invalid_attachment_data(
                "attachment repository returned a row outside delete Requirement",
            ));
        }
        let disk_name = validated_attachment_disk_name(row)?;
        let Some(source) = resolve_attachment_source(data_dir, row)? else {
            continue;
        };
        let source = open_stable_regular_file(
            &source,
            "attachment source before delete journal",
        )?;
        let identity = file_identity(&source)?;
        entries.push(AttachmentDeleteJournalEntry {
            attachment_id: row.attachment_id.clone(),
            staged_name: expected_delete_staged_name(
                &disk_name,
                &operation_id,
            ),
            disk_name,
            identity_volume: identity.volume,
            identity_index: identity.index,
        });
    }
    if entries.is_empty() {
        return Ok(());
    }

    let journal = AttachmentDeleteJournal {
        version: DELETE_JOURNAL_VERSION,
        requirement_id: requirement_id.to_owned(),
        operation_id,
        entries,
    };
    validate_delete_journal(&journal, requirement_id, rows)?;
    let bytes = serde_json::to_vec(&journal).map_err(|error| {
        invalid_attachment_data(format!(
            "serialize attachment delete journal: {error}"
        ))
    })?;
    publish_static_file(
        &requirement_root,
        DELETE_JOURNAL_FILE,
        &bytes,
    )?;
    sync_directory(&requirement_root)?;

    for entry in &journal.entries {
        if let Err(stage_error) =
            stage_delete_journal_entry(&requirement_root, entry)
        {
            let recovery =
                reconcile_delete_journal_blocking(data_dir, requirement_id, rows);
            return match recovery {
                Ok(()) => Err(stage_error),
                Err(recovery_error) => Err(invalid_attachment_data(format!(
                    "attachment delete staging failed ({stage_error}); durable rollback also failed ({recovery_error})"
                ))),
            };
        }
    }
    Ok(())
}

fn validate_staging_directory(directory: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(invalid_attachment_data(
                "attachment staging directory contains a non-UTF-8 entry",
            ));
        };
        let Some((attachment_id, extension)) = name.rsplit_once('.') else {
            return Err(invalid_attachment_data(format!(
                "attachment staging directory contains unexpected entry '{name}'"
            )));
        };
        if AttachmentId::parse(attachment_id.to_owned()).is_err()
            || extension != extension.to_ascii_lowercase()
            || !IMAGE_EXTENSIONS.contains(&extension)
        {
            return Err(invalid_attachment_data(format!(
                "attachment staging directory contains non-canonical entry '{name}'"
            )));
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        validate_plain_metadata(
            &metadata,
            PlainPathKind::File,
            "attachment staging directory entry",
        )?;
        drop(open_stable_regular_file(
            &entry.path(),
            "attachment staging directory entry",
        )?);
    }
    Ok(())
}

fn activate_prompt_plan_blocking(
    data_dir: &Path,
    workspace: &Path,
    copies: &[PromptAttachmentCopy],
) -> io::Result<()> {
    if workspace_path_has_edge_whitespace_segment(workspace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "workspace has a component with leading or trailing whitespace: {}",
                workspace.display()
            ),
        ));
    }
    let workspace_metadata = std::fs::symlink_metadata(workspace)?;
    validate_plain_metadata(
        &workspace_metadata,
        PlainPathKind::Directory,
        "workspace root",
    )?;
    let workspace = std::fs::canonicalize(workspace)?;
    validate_plain_metadata(
        &std::fs::symlink_metadata(&workspace)?,
        PlainPathKind::Directory,
        "physical workspace root",
    )?;
    let nomi_root =
        ensure_plain_directory(&workspace, OsStr::new(".nomi"), "workspace .nomi directory")?;
    let stage_root = ensure_plain_directory(
        &nomi_root,
        OsStr::new("requirement-attachments"),
        "workspace attachment staging root",
    )?;
    publish_static_file(&stage_root, ".gitignore", b"*\n")?;

    let mut requirement_directories: HashMap<String, PathBuf> = HashMap::new();
    for copy in copies {
        RequirementId::parse(copy.row.requirement_id.clone()).map_err(|error| {
            invalid_attachment_data(format!(
                "attachment copy has a non-canonical requirement identity: {error}"
            ))
        })?;
        let validated_disk_name = validated_attachment_disk_name(&copy.row)?;
        if validated_disk_name != copy.disk_name {
            return Err(invalid_attachment_data(
                "attachment copy identity changed after planning",
            ));
        }
        let source = resolve_attachment_source(data_dir, &copy.row)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "persisted attachment disappeared after planning",
            )
        })?;
        let requirement_dir =
            if let Some(directory) = requirement_directories.get(&copy.row.requirement_id) {
                directory.clone()
            } else {
                let directory = ensure_plain_directory(
                    &stage_root,
                    OsStr::new(&copy.row.requirement_id),
                    "workspace attachment requirement directory",
                )?;
                validate_staging_directory(&directory)?;
                requirement_directories.insert(copy.row.requirement_id.clone(), directory.clone());
                directory
            };
        publish_attachment_copy(&source, &requirement_dir, &copy.disk_name)?;
    }
    Ok(())
}

pub(crate) struct PreparedAttachmentDelete {
    recovery: Option<PreparedDeleteRecovery>,
}

struct PreparedDeleteRecovery {
    data_dir: PathBuf,
    requirement_id: String,
    repo: Arc<dyn IAttachmentRepository>,
    mutation_guard: tokio::sync::OwnedMutexGuard<()>,
}

fn validate_requirement_id(requirement_id: &str) -> Result<(), AppError> {
    RequirementId::parse(requirement_id)
        .map(|_| ())
        .map_err(|error| AppError::BadRequest(format!("invalid requirement id: {error}")))
}

/// Lowercased extension when it is in the image whitelist.
fn image_ext(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// `name(2).ext` display-name dedup within one target (mirrors the upload
/// service's numeric-suffix pattern).
fn unique_name(want: &str, used: &[String]) -> String {
    if !used.iter().any(|u| u == want) {
        return want.to_string();
    }
    let (base, ext) = match want.rfind('.') {
        Some(i) if i > 0 => (&want[..i], &want[i..]),
        _ => (want, ""),
    };
    for n in 2..1000 {
        let candidate = format!("{base}({n}){ext}");
        if !used.iter().any(|u| u == &candidate) {
            return candidate;
        }
    }
    format!("{base}({}){ext}", generate_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{
        IAttachmentRepository, IRequirementRepository, SqliteAttachmentRepository,
        SqliteRequirementRepository, init_database_memory,
    };
    use std::sync::{Arc, Condvar};
    use std::time::Duration;

    const REQ_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const REQ_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const USER_1: &str = "0190f5fe-7c00-7a00-8000-000000000003";
    const CONVERSATION_1: &str = "0190f5fe-7c00-7a00-8000-000000000010";

    async fn store() -> (AttachmentStore, tempfile::TempDir, tempfile::TempDir) {
        let (store, data_dir, upload_root, _pool) = store_with_pool().await;
        (store, data_dir, upload_root)
    }

    async fn store_with_pool() -> (
        AttachmentStore,
        tempfile::TempDir,
        tempfile::TempDir,
        sqlx::SqlitePool,
    ) {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO conversations \
             (conversation_id, user_id, name, type, status, created_at, updated_at) \
             VALUES (?, ?, 'test', 'nomi', 'pending', 0, 0)",
        )
        .bind(CONVERSATION_1)
        .bind(USER_1)
        .execute(db.pool())
        .await
        .unwrap();
        // Seed the parent rows used by the logical attachments.requirement_id
        // references. SQLite does not enforce or cascade these links.
        for (index, requirement_id) in [REQ_1, REQ_2].into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO requirements \
                 (requirement_id, display_no, title, content, tag, order_key, sort_seq, status, priority, attempt_count, created_by, extra, created_at, updated_at) \
                 VALUES (?, ?, 'T', '', 't', '', '', 'pending', 0, 0, 'user', '{}', 0, 0)",
            )
            .bind(requirement_id)
            .bind((index + 1) as i64)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let pool = db.pool().clone();
        let repo: Arc<dyn nomifun_db::IAttachmentRepository> =
            Arc::new(SqliteAttachmentRepository::new(pool.clone()));
        Box::leak(Box::new(db));
        let data_dir = tempfile::tempdir().unwrap();
        let upload_root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(data_dir.path().to_path_buf(), repo)
            .with_upload_root(upload_root.path().to_path_buf());
        (store, data_dir, upload_root, pool)
    }

    fn put_upload(root: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        let p = root.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    fn assert_no_delete_artifacts(data_dir: &Path, requirement_id: &str) {
        let requirement_dir = data_dir
            .join(ATTACHMENTS_REL_DIR)
            .join(requirement_id);
        let Ok(entries) = std::fs::read_dir(requirement_dir) else {
            return;
        };
        for entry in entries {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert_ne!(name, DELETE_JOURNAL_FILE);
            assert!(
                !name.contains(".deleting-"),
                "unexpected attachment delete quarantine survived: {name}"
            );
        }
    }

    #[cfg(windows)]
    fn create_directory_junction(link: &Path, target: &Path) {
        use std::os::windows::process::CommandExt;

        let link = link.to_str().expect("junction link path is UTF-8");
        let target = target.to_str().expect("junction target path is UTF-8");
        assert!(!link.contains('"') && !target.contains('"'));
        let command = format!(r#"mklink /J "{link}" "{target}""#);
        let status = std::process::Command::new("cmd.exe")
            .args(["/d", "/c"])
            .raw_arg(&command)
            .status()
            .expect("start mklink junction");
        assert!(
            status.success(),
            "create Windows directory junction with command: {command}"
        );
    }

    #[tokio::test]
    async fn ingest_copies_into_data_dir_and_inserts_rows() {
        let (store, data_dir, upload_root) = store().await;
        let src = put_upload(upload_root.path(), "shot.png", b"png-bytes");
        let rows = store
            .ingest(REQ_1, &[NewAttachmentRef { source_path: src, file_name: "设计稿.png".into() }], Some("user"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(AttachmentId::try_from(row.attachment_id.as_str()).is_ok());
        assert_eq!(row.file_name, "设计稿.png");
        assert_eq!(row.mime, "image/png");
        assert_eq!(row.size_bytes, 9);
        // file landed at data_dir/rel_path with the att id as disk name
        let abs = data_dir.path().join(&row.rel_path);
        assert!(abs.exists());
        assert!(row.rel_path.starts_with(&format!("attachments/{REQ_1}/")));
        assert!(row.rel_path.ends_with(".png"));
        // listed back
        assert_eq!(store.list(REQ_1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ingest_rejects_non_image_and_traversal_and_outside_root() {
        let (store, _data, upload_root) = store().await;
        // non-image extension
        let txt = put_upload(upload_root.path(), "a.txt", b"x");
        let err = store
            .ingest(REQ_1, &[NewAttachmentRef { source_path: txt, file_name: "a.txt".into() }], None)
            .await
            .unwrap_err();
        assert!(matches!(err, nomifun_common::AppError::BadRequest(_)));
        // traversal in source path
        let err = store
            .ingest(REQ_1, &[NewAttachmentRef { source_path: "../../etc/passwd.png".into(), file_name: "p.png".into() }], None)
            .await
            .unwrap_err();
        assert!(matches!(err, nomifun_common::AppError::BadRequest(_) | nomifun_common::AppError::Forbidden(_)));
        // exists but outside upload root
        let outside = tempfile::tempdir().unwrap();
        let out = put_upload(outside.path(), "b.png", b"x");
        let err = store
            .ingest(REQ_1, &[NewAttachmentRef { source_path: out, file_name: "b.png".into() }], None)
            .await
            .unwrap_err();
        assert!(matches!(err, nomifun_common::AppError::Forbidden(_)));
        // nothing was inserted by the failed batches
        assert!(store.list(REQ_1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ingest_failure_mid_batch_cleans_up_earlier_copies() {
        let (store, data_dir, upload_root) = store().await;
        let ok = put_upload(upload_root.path(), "ok.png", b"x");
        let rows = store
            .ingest(
                REQ_1,
                &[
                    NewAttachmentRef { source_path: ok, file_name: "ok.png".into() },
                    NewAttachmentRef { source_path: upload_root.path().join("missing.png").to_string_lossy().into(), file_name: "missing.png".into() },
                ],
                None,
            )
            .await;
        assert!(rows.is_err());
        assert!(store.list(REQ_1).await.unwrap().is_empty(), "no rows survive a failed batch");
        let dir = data_dir.path().join(format!("attachments/{REQ_1}"));
        let leftover = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftover, 0, "copied files from the failed batch are cleaned up");
    }

    #[tokio::test]
    async fn file_name_dedup_within_target() {
        let (store, _data, upload_root) = store().await;
        let a = put_upload(upload_root.path(), "a.png", b"x");
        let b = put_upload(upload_root.path(), "b.png", b"y");
        store.ingest(REQ_1, &[NewAttachmentRef { source_path: a, file_name: "img.png".into() }], None).await.unwrap();
        let rows = store.ingest(REQ_1, &[NewAttachmentRef { source_path: b, file_name: "img.png".into() }], None).await.unwrap();
        assert_eq!(rows[0].file_name, "img(2).png");
    }

    #[tokio::test]
    async fn remove_and_delete_all_clean_rows_and_files() {
        let (store, data_dir, upload_root) = store().await;
        let a = put_upload(upload_root.path(), "a.png", b"x");
        let b = put_upload(upload_root.path(), "b.png", b"y");
        let rows = store
            .ingest(
                REQ_1,
                &[
                    NewAttachmentRef { source_path: a, file_name: "a.png".into() },
                    NewAttachmentRef { source_path: b, file_name: "b.png".into() },
                ],
                None,
            )
            .await
            .unwrap();
        // Remove by stable attachment business ID; the local row ID stays
        // internal to persistence.
        store
            .remove(REQ_1, &[rows[0].attachment_id.clone()])
            .await
            .unwrap();
        assert_eq!(store.list(REQ_1).await.unwrap().len(), 1);
        assert!(!data_dir.path().join(&rows[0].rel_path).exists());
        // remove with an id belonging to ANOTHER requirement is a no-op (scope guard)
        store
            .remove(REQ_2, &[rows[1].attachment_id.clone()])
            .await
            .unwrap();
        assert_eq!(store.list(REQ_1).await.unwrap().len(), 1);
        // delete_all —everything gone including the dir
        store.delete_all(REQ_1).await.unwrap();
        assert!(store.list(REQ_1).await.unwrap().is_empty());
        assert!(!data_dir.path().join(format!("attachments/{REQ_1}")).exists());
    }

    #[tokio::test]
    async fn active_requirement_delete_conflict_restores_journaled_attachments() {
        let (store, data_dir, upload_root, pool) = store_with_pool().await;
        let source = put_upload(upload_root.path(), "active.png", b"active");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "active.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);
        let requirement_repo = SqliteRequirementRepository::new(pool);
        let claim = requirement_repo
            .claim_next_for_runner(
                "t",
                Some(CONVERSATION_1),
                None,
                60_000,
                now_ms(),
            )
            .await
            .unwrap();
        assert!(claim.is_some(), "seeded Requirement must be claimed");

        let prepared = store.prepare_delete_all(REQ_1).await.unwrap();
        let delete_error = requirement_repo.delete(REQ_1).await.unwrap_err();
        assert!(matches!(delete_error, nomifun_db::DbError::Conflict(_)));
        store.restore_prepared_delete(prepared).await.unwrap();

        let requirement = requirement_repo
            .get_by_requirement_id(REQ_1)
            .await
            .unwrap()
            .expect("active Requirement must not be deleted");
        assert_eq!(requirement.status, "needs_review");
        let persisted_rows = store.list(REQ_1).await.unwrap();
        assert_eq!(persisted_rows.len(), 1);
        assert_eq!(persisted_rows[0].attachment_id, row.attachment_id);
        assert_eq!(
            std::fs::read(data_dir.path().join(&row.rel_path)).unwrap(),
            b"active"
        );
        assert_no_delete_artifacts(data_dir.path(), REQ_1);
    }

    #[tokio::test]
    async fn aborted_prepare_recovers_from_durable_journal_before_next_access() {
        let (mut store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "abort.png", b"abort");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "abort.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);

        let gate = Arc::new((StdMutex::new((false, false)), Condvar::new()));
        let cutpoint_gate = Arc::clone(&gate);
        store.delete_stage_cutpoint = Some(Arc::new(move || {
            let (lock, wake) = &*cutpoint_gate;
            let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
            state.0 = true;
            wake.notify_all();
            let (state_after_wait, wait_result) = wake
                .wait_timeout_while(state, Duration::from_secs(10), |state| !state.1)
                .unwrap_or_else(|error| error.into_inner());
            assert!(
                !wait_result.timed_out() && state_after_wait.1,
                "test did not release attachment delete cutpoint"
            );
        }));
        let store = Arc::new(store);
        let prepare_task = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.prepare_delete_all(REQ_1).await }
        });
        let observed_gate = Arc::clone(&gate);
        tokio::task::spawn_blocking(move || {
            let (lock, wake) = &*observed_gate;
            let state = lock.lock().unwrap_or_else(|error| error.into_inner());
            let (state, wait_result) = wake
                .wait_timeout_while(state, Duration::from_secs(10), |state| !state.0)
                .unwrap_or_else(|error| error.into_inner());
            assert!(
                !wait_result.timed_out() && state.0,
                "attachment delete staging cutpoint was not reached"
            );
        })
        .await
        .unwrap();

        let requirement_dir = data_dir
            .path()
            .join(ATTACHMENTS_REL_DIR)
            .join(REQ_1);
        assert!(requirement_dir.join(DELETE_JOURNAL_FILE).exists());
        assert!(!data_dir.path().join(&row.rel_path).exists());
        prepare_task.abort();
        {
            let (lock, wake) = &*gate;
            let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
            state.1 = true;
            wake.notify_all();
        }
        match prepare_task.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("aborted attachment prepare unexpectedly completed"),
        }

        let persisted_rows = store.list(REQ_1).await.unwrap();
        assert_eq!(persisted_rows.len(), 1);
        assert_eq!(persisted_rows[0].attachment_id, row.attachment_id);
        assert_eq!(
            std::fs::read(data_dir.path().join(&row.rel_path)).unwrap(),
            b"abort"
        );
        assert_no_delete_artifacts(data_dir.path(), REQ_1);
    }

    #[tokio::test]
    async fn dropped_prepared_after_commit_reconciles_without_future_access() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "cancel-after-commit.png", b"committed");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "cancel-after-commit.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);
        let requirement_dir = data_dir
            .path()
            .join(ATTACHMENTS_REL_DIR)
            .join(REQ_1);

        let prepared = store.prepare_delete_all(REQ_1).await.unwrap();
        store.repo.delete_for_requirement(REQ_1).await.unwrap();
        drop(prepared);
        tokio::time::timeout(Duration::from_secs(10), async {
            while requirement_dir.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled committed delete was not reconciled in the live runtime");

        assert!(
            store
                .repo
                .list_for_requirement(REQ_1)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!data_dir.path().join(&row.rel_path).exists());
        assert_no_delete_artifacts(data_dir.path(), REQ_1);
    }

    #[tokio::test]
    async fn concurrent_delete_retry_waits_for_owner_and_leaves_no_orphan() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "concurrent.png", b"concurrent");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "concurrent.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);
        let store = Arc::new(store);

        let first = store.prepare_delete_all(REQ_1).await.unwrap();
        let second_task = tokio::spawn({
            let store = Arc::clone(&store);
            async move { store.prepare_delete_all(REQ_1).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !second_task.is_finished(),
            "a retry must wait for the first delete transaction"
        );

        store.restore_prepared_delete(first).await.unwrap();
        let second = match tokio::time::timeout(Duration::from_secs(10), second_task).await {
            Ok(Ok(Ok(prepared))) => prepared,
            Ok(Ok(Err(error))) => panic!("second attachment prepare failed: {error}"),
            Ok(Err(error)) => panic!("second attachment prepare task failed: {error}"),
            Err(_) => panic!("second attachment prepare stayed blocked"),
        };
        store.restore_prepared_delete(second).await.unwrap();

        let persisted_rows = store.list(REQ_1).await.unwrap();
        assert_eq!(persisted_rows.len(), 1);
        assert_eq!(persisted_rows[0].attachment_id, row.attachment_id);
        assert_eq!(
            std::fs::read(data_dir.path().join(&row.rel_path)).unwrap(),
            b"concurrent"
        );
        assert_no_delete_artifacts(data_dir.path(), REQ_1);
    }

    #[tokio::test]
    async fn boot_sweep_finishes_delete_committed_before_process_interruption() {
        let (store, data_dir, upload_root, pool) = store_with_pool().await;
        let source = put_upload(upload_root.path(), "crash.png", b"crash");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "crash.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);

        let prepared = store.prepare_delete_all(REQ_1).await.unwrap();
        store.repo.delete_for_requirement(REQ_1).await.unwrap();
        prepared.simulate_process_interruption();
        let requirement_dir = data_dir
            .path()
            .join(ATTACHMENTS_REL_DIR)
            .join(REQ_1);
        assert!(requirement_dir.join(DELETE_JOURNAL_FILE).exists());
        assert!(!data_dir.path().join(&row.rel_path).exists());

        let recovery_repo: Arc<dyn IAttachmentRepository> =
            Arc::new(SqliteAttachmentRepository::new(pool));
        let recovery_store =
            AttachmentStore::new(data_dir.path().to_path_buf(), recovery_repo);
        recovery_store.recover_pending_deletes().await.unwrap();

        assert!(
            recovery_store
                .repo
                .list_for_requirement(REQ_1)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!requirement_dir.exists());
        assert_no_delete_artifacts(data_dir.path(), REQ_1);
    }

    #[tokio::test]
    async fn boot_sweep_recovers_both_journal_publish_crash_states() {
        let (store, data_dir, upload_root, pool) = store_with_pool().await;
        let source = put_upload(upload_root.path(), "publish-crash.png", b"publish-crash");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "publish-crash.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);
        let requirement_dir = data_dir
            .path()
            .join(ATTACHMENTS_REL_DIR)
            .join(REQ_1);
        let orphan_temp_name = format!(
            ".{DELETE_JOURNAL_FILE}.staging-{}",
            AttachmentId::new()
        );
        std::fs::write(requirement_dir.join(&orphan_temp_name), b"incomplete").unwrap();

        let recovery_repo: Arc<dyn IAttachmentRepository> =
            Arc::new(SqliteAttachmentRepository::new(pool));
        let recovery_store =
            AttachmentStore::new(data_dir.path().to_path_buf(), recovery_repo);
        recovery_store.recover_pending_deletes().await.unwrap();
        assert!(!requirement_dir.join(orphan_temp_name).exists());
        assert!(data_dir.path().join(&row.rel_path).exists());

        let prepared = store.prepare_delete_all(REQ_1).await.unwrap();
        let journal_path = requirement_dir.join(DELETE_JOURNAL_FILE);
        let linked_temp_name = format!(
            ".{DELETE_JOURNAL_FILE}.staging-{}",
            AttachmentId::new()
        );
        std::fs::hard_link(&journal_path, requirement_dir.join(&linked_temp_name)).unwrap();
        prepared.simulate_process_interruption();

        recovery_store.recover_pending_deletes().await.unwrap();
        assert!(!requirement_dir.join(linked_temp_name).exists());
        assert!(!journal_path.exists());
        assert_eq!(
            std::fs::read(data_dir.path().join(&row.rel_path)).unwrap(),
            b"publish-crash"
        );
        assert_no_delete_artifacts(data_dir.path(), REQ_1);
    }

    #[tokio::test]
    async fn boot_sweep_rejects_noncanonical_journal_publish_alias() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "alias.png", b"alias");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "alias.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);
        let requirement_dir = data_dir
            .path()
            .join(ATTACHMENTS_REL_DIR)
            .join(REQ_1);
        let alias_name = format!(
            ".{DELETE_JOURNAL_FILE}.staging-{}",
            AttachmentId::new()
        )
        .to_ascii_uppercase();
        std::fs::write(requirement_dir.join(&alias_name), b"alias").unwrap();

        assert!(store.recover_pending_deletes().await.is_err());
        let alias_path = requirement_dir.join(alias_name);
        assert!(alias_path.exists());
        assert_eq!(
            std::fs::read(data_dir.path().join(&row.rel_path)).unwrap(),
            b"alias"
        );

        std::fs::remove_file(alias_path).unwrap();
        let quarantine_alias = format!(
            ".{}.PNG.DELETING-{}",
            row.attachment_id,
            AttachmentId::new()
        );
        std::fs::write(requirement_dir.join(&quarantine_alias), b"alias").unwrap();
        assert!(store.recover_pending_deletes().await.is_err());
        assert!(requirement_dir.join(quarantine_alias).exists());
    }

    #[tokio::test]
    async fn boot_sweep_rejects_noncanonical_requirement_storage_children() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "root-child.png", b"root-child");
        store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "root-child.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let unexpected = data_dir
            .path()
            .join(ATTACHMENTS_REL_DIR)
            .join("NOT-A-REQUIREMENT");
        std::fs::create_dir(&unexpected).unwrap();
        std::fs::write(
            unexpected.join(DELETE_JOURNAL_FILE),
            b"must not be silently skipped",
        )
        .unwrap();

        assert!(store.recover_pending_deletes().await.is_err());
        assert!(unexpected.join(DELETE_JOURNAL_FILE).exists());
    }

    #[tokio::test]
    async fn missing_storage_root_does_not_strand_attachment_rows() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "orphan.png", b"orphan");
        let row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "orphan.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);

        std::fs::remove_dir_all(data_dir.path()).unwrap();
        store
            .remove(REQ_1, std::slice::from_ref(&row.attachment_id))
            .await
            .unwrap();
        assert!(store.list(REQ_1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stage_for_prompt_uses_canonical_disk_name_and_never_falls_back() {
        let (store, data_dir, upload_root) = store().await;
        let a = put_upload(upload_root.path(), "a.png", b"x");
        let rows = store
            .ingest(REQ_1, &[NewAttachmentRef { source_path: a, file_name: "图片.png".into() }], None)
            .await
            .unwrap();
        // Workspace staging always uses the canonical attachment ID, never the
        // user-controlled display name.
        let ws = tempfile::tempdir().unwrap();
        let staged = store.stage_for_prompt(REQ_1, Some(ws.path())).await;
        assert_eq!(staged.len(), 1);
        assert!(!staged[0].missing);
        assert_eq!(
            staged[0].path,
            format!(
                "./{WORKSPACE_STAGE_REL_DIR}/{REQ_1}/{}.png",
                rows[0].attachment_id
            )
        );
        assert!(
            ws.path()
                .join(staged[0].path.trim_start_matches("./"))
                .exists()
        );
        assert!(ws.path().join(".nomi/requirement-attachments/.gitignore").exists());
        // No workspace uses the verified physical original path.
        let staged = store.stage_for_prompt(REQ_1, None).await;
        assert_eq!(
            PathBuf::from(&staged[0].path),
            std::fs::canonicalize(data_dir.path().join(&rows[0].rel_path)).unwrap()
        );
        // A vanished original is represented as missing, never as a stale path.
        std::fs::remove_file(data_dir.path().join(&rows[0].rel_path)).unwrap();
        let staged = store.stage_for_prompt(REQ_1, Some(ws.path())).await;
        assert!(staged[0].missing);
    }

    #[tokio::test]
    async fn prompt_plan_is_read_only_and_activation_preserves_exact_paths() {
        let (store, _data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "planned.png", b"planned");
        let rows = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "planned.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let stage_root = workspace.path().join(WORKSPACE_STAGE_REL_DIR);

        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();
        assert_eq!(
            plan.attachments,
            vec![PromptAttachment {
                file_name: "planned.png".into(),
                path: format!(
                    "./{WORKSPACE_STAGE_REL_DIR}/{REQ_1}/{}.png",
                    rows[0].attachment_id
                ),
                missing: false,
            }]
        );
        assert!(
            !stage_root.exists(),
            "read-only planning must not create or modify the workspace"
        );

        store.activate_prompt_plan(&plan).await.unwrap();
        assert!(
            workspace
                .path()
                .join(
                    plan.attachments[0]
                        .path
                        .trim_start_matches("./"),
                )
                .exists()
        );
        assert!(stage_root.join(".gitignore").exists());
    }

    #[tokio::test]
    async fn hostile_display_names_never_become_workspace_path_components() {
        let (store, _data_dir, upload_root) = store().await;
        let display_names = [
            "../escape.png",
            r"..\escape.png",
            "/tmp/absolute.png",
            r"C:\Windows\system32.png",
            r"\\server\share\remote.png",
            "CON.png",
            "trailing.png. ",
            "case.png",
            "CASE.png",
        ];
        let mut refs = Vec::with_capacity(display_names.len());
        for (index, display_name) in display_names.iter().enumerate() {
            let source_name = format!("safe-{index}.png");
            refs.push(NewAttachmentRef {
                source_path: put_upload(
                    upload_root.path(),
                    &source_name,
                    format!("bytes-{index}").as_bytes(),
                ),
                file_name: (*display_name).to_owned(),
            });
        }
        let rows = store.ingest(REQ_1, &refs, None).await.unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();

        assert_eq!(plan.attachments.len(), display_names.len());
        for ((attachment, copy), display_name) in plan
            .attachments
            .iter()
            .zip(&plan.copies)
            .zip(display_names)
        {
            let expected_disk_name = format!("{}.png", copy.row.attachment_id);
            assert_eq!(attachment.file_name, display_name);
            assert_eq!(
                attachment.path,
                format!("./{WORKSPACE_STAGE_REL_DIR}/{REQ_1}/{expected_disk_name}")
            );
            assert!(!attachment.path.contains("../"));
            assert!(!attachment.path.contains(r"..\"));
        }

        store.activate_prompt_plan(&plan).await.unwrap();
        let requirement_dir = workspace
            .path()
            .join(WORKSPACE_STAGE_REL_DIR)
            .join(REQ_1);
        let mut actual_names = std::fs::read_dir(&requirement_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual_names.sort();
        let mut expected_names = rows
            .iter()
            .map(|row| format!("{}.png", row.attachment_id))
            .collect::<Vec<_>>();
        expected_names.sort();
        assert_eq!(actual_names, expected_names);
        assert!(
            !workspace
                .path()
                .join(WORKSPACE_STAGE_REL_DIR)
                .join("escape.png")
                .exists()
        );
    }

    #[test]
    fn persisted_rel_path_must_have_one_exact_portable_shape() {
        let attachment_id = AttachmentId::new().into_string();
        let canonical_disk_name = format!("{attachment_id}.png");
        let mut row = AttachmentRow {
            id: 1,
            attachment_id,
            requirement_id: REQ_1.to_owned(),
            file_name: "display-only.png".to_owned(),
            rel_path: format!("{ATTACHMENTS_REL_DIR}/{REQ_1}/{canonical_disk_name}"),
            mime: "image/png".to_owned(),
            size_bytes: 1,
            created_by: None,
            created_at: 0,
        };
        assert_eq!(
            validated_attachment_disk_name(&row).unwrap(),
            canonical_disk_name
        );

        let invalid_paths = [
            format!("../{canonical_disk_name}"),
            format!("{ATTACHMENTS_REL_DIR}/{REQ_1}/../{canonical_disk_name}"),
            format!("./{ATTACHMENTS_REL_DIR}/{REQ_1}/{canonical_disk_name}"),
            format!("{ATTACHMENTS_REL_DIR}//{REQ_1}/{canonical_disk_name}"),
            format!(r"{ATTACHMENTS_REL_DIR}\{REQ_1}\{canonical_disk_name}"),
            format!("/tmp/{canonical_disk_name}"),
            format!(r"C:\temp\{canonical_disk_name}"),
            format!(r"\\server\share\{canonical_disk_name}"),
            format!("{ATTACHMENTS_REL_DIR}/{REQ_2}/{canonical_disk_name}"),
            format!(
                "{ATTACHMENTS_REL_DIR}/{REQ_1}/{}.PNG",
                row.attachment_id
            ),
            format!("{ATTACHMENTS_REL_DIR}/{REQ_1}/CON.png"),
        ];
        for invalid_path in invalid_paths {
            row.rel_path = invalid_path.clone();
            assert!(
                validated_attachment_disk_name(&row).is_err(),
                "unexpectedly accepted {invalid_path:?}"
            );
        }
    }

    #[tokio::test]
    async fn tampered_database_rel_path_is_rejected_without_absolute_fallback() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "tampered.png", b"original");
        let mut row = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "safe.png".into(),
                }],
                None,
            )
            .await
            .unwrap()
            .remove(0);
        store.repo.delete(row.id).await.unwrap();
        row.id = 0;
        row.rel_path = format!("../{}.png", row.attachment_id);
        store.repo.insert(&row).await.unwrap();

        let workspace = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_for_prompt(REQ_1, Some(workspace.path()))
                .await
                .is_err()
        );
        let legacy = store.stage_for_prompt(REQ_1, Some(workspace.path())).await;
        assert!(
            legacy.is_empty(),
            "legacy wrapper must fail closed, not reveal an absolute fallback"
        );
        assert!(!workspace.path().join(".nomi").exists());
        assert!(
            store
                .abs_path(&row)
                .starts_with(data_dir.path().join(ATTACHMENTS_REL_DIR)),
            "even display DTO fallback paths must remain inside attachment storage"
        );
        assert!(
            store
                .remove(REQ_1, std::slice::from_ref(&row.attachment_id))
                .await
                .is_err(),
            "removal must validate the same canonical persisted shape"
        );
        assert_eq!(store.list(REQ_1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn activation_is_idempotent_but_never_overwrites_different_content() {
        let (store, _data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "idempotent.png", b"trusted");
        store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "idempotent.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();
        store.activate_prompt_plan(&plan).await.unwrap();
        store.activate_prompt_plan(&plan).await.unwrap();

        let destination = workspace
            .path()
            .join(plan.attachments[0].path.trim_start_matches("./"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"trusted");
        std::fs::write(&destination, b"attacker-content").unwrap();
        assert!(store.activate_prompt_plan(&plan).await.is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"attacker-content",
            "no-clobber publication must never overwrite an existing file"
        );
        let parent = destination.parent().unwrap();
        assert!(
            std::fs::read_dir(parent).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".staging-")
            }),
            "failed publication must clean only the temporary file it owns"
        );
    }

    #[tokio::test]
    async fn source_and_destination_hardlinks_fail_closed() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "hardlink.png", b"trusted");
        let rows = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "hardlink.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let persisted = data_dir.path().join(&rows[0].rel_path);
        let source_alias = data_dir.path().join("source-hardlink-alias.png");
        std::fs::hard_link(&persisted, &source_alias).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_for_prompt(REQ_1, Some(workspace.path()))
                .await
                .is_err(),
            "a hard-linked persistent source must not be trusted"
        );
        assert!(
            store.prepare_delete_all(REQ_1).await.is_err(),
            "delete must reject the same hard-linked persistent source"
        );
        std::fs::remove_file(source_alias).unwrap();

        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();
        store.activate_prompt_plan(&plan).await.unwrap();
        let destination = workspace
            .path()
            .join(plan.attachments[0].path.trim_start_matches("./"));
        std::fs::remove_file(&destination).unwrap();
        let victim = workspace.path().join("victim.png");
        std::fs::write(&victim, b"victim").unwrap();
        std::fs::hard_link(&victim, &destination).unwrap();

        assert!(store.activate_prompt_plan(&plan).await.is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim");
        assert_eq!(std::fs::read(&destination).unwrap(), b"victim");
    }

    #[tokio::test]
    async fn case_colliding_persisted_disk_name_fails_closed() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "source-case.png", b"trusted");
        let rows = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "display.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let persisted = data_dir.path().join(&rows[0].rel_path);
        let upper_case_name = persisted
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_uppercase();
        let upper_case_path = persisted.with_file_name(upper_case_name);
        std::fs::rename(&persisted, &upper_case_path).unwrap();

        let workspace = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_for_prompt(REQ_1, Some(workspace.path()))
                .await
                .is_err()
        );
        assert!(
            store.prepare_delete_all(REQ_1).await.is_err(),
            "delete must reject the same symbolic-link source"
        );
        assert!(!workspace.path().join(".nomi").exists());
    }

    #[tokio::test]
    async fn case_colliding_workspace_component_fails_on_every_platform() {
        let (store, _data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "case-alias.png", b"trusted");
        store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "case-alias.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".NOMI")).unwrap();
        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();
        assert!(store.activate_prompt_plan(&plan).await.is_err());
        assert!(!workspace.path().join(".NOMI/requirement-attachments").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_canonical_containment_does_not_case_fold() {
        assert!(!paths_equal_for_platform(
            Path::new("/private/tmp/CaseSensitive"),
            Path::new("/private/tmp/casesensitive")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_workspace_symlink_component_is_rejected() {
        use std::os::unix::fs::symlink;

        let (store, _data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "symlink-target.png", b"trusted");
        store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "symlink-target.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".nomi")).unwrap();
        symlink(
            outside.path(),
            workspace.path().join(".nomi/requirement-attachments"),
        )
        .unwrap();
        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();
        assert!(store.activate_prompt_plan(&plan).await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_persisted_source_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "symlink-source.png", b"trusted");
        let rows = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "symlink-source.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let persisted = data_dir.path().join(&rows[0].rel_path);
        let victim = data_dir.path().join("outside-victim.png");
        std::fs::write(&victim, b"victim").unwrap();
        std::fs::remove_file(&persisted).unwrap();
        symlink(&victim, &persisted).unwrap();

        let workspace = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_for_prompt(REQ_1, Some(workspace.path()))
                .await
                .is_err()
        );
        assert!(
            store.prepare_delete_all(REQ_1).await.is_err(),
            "delete must reject the same junction-backed source"
        );
        assert!(!workspace.path().join(".nomi").exists());
        assert_eq!(std::fs::read(victim).unwrap(), b"victim");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_workspace_reparse_component_is_rejected() {
        use std::os::windows::fs::symlink_dir;

        let (store, _data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "reparse-target.png", b"trusted");
        store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "reparse-target.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".nomi")).unwrap();
        let reparse_path = workspace.path().join(".nomi/requirement-attachments");
        if let Err(error) = symlink_dir(outside.path(), &reparse_path) {
            // Directory junctions exercise the same reparse-point gate and do
            // not require SeCreateSymbolicLinkPrivilege on older Windows hosts.
            if error.kind() != io::ErrorKind::PermissionDenied
                && error.raw_os_error() != Some(1314)
            {
                panic!("create Windows directory reparse point: {error}");
            }
            create_directory_junction(&reparse_path, outside.path());
        }
        let plan = store
            .plan_for_prompt(REQ_1, Some(workspace.path()))
            .await
            .unwrap();
        assert!(store.activate_prompt_plan(&plan).await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_persisted_source_junction_is_rejected() {
        let (store, data_dir, upload_root) = store().await;
        let source = put_upload(upload_root.path(), "junction-source.png", b"trusted");
        let rows = store
            .ingest(
                REQ_1,
                &[NewAttachmentRef {
                    source_path: source,
                    file_name: "junction-source.png".into(),
                }],
                None,
            )
            .await
            .unwrap();
        let persisted = data_dir.path().join(&rows[0].rel_path);
        let requirement_dir = persisted.parent().unwrap().to_path_buf();
        let outside = tempfile::tempdir().unwrap();
        let outside_source = outside.path().join(persisted.file_name().unwrap());
        std::fs::rename(&persisted, &outside_source).unwrap();
        std::fs::remove_dir(&requirement_dir).unwrap();
        create_directory_junction(&requirement_dir, outside.path());

        let workspace = tempfile::tempdir().unwrap();
        assert!(
            store
                .plan_for_prompt(REQ_1, Some(workspace.path()))
                .await
                .is_err()
        );
        assert!(!workspace.path().join(".nomi").exists());
        assert_eq!(std::fs::read(&outside_source).unwrap(), b"trusted");
        std::fs::remove_dir(&requirement_dir).unwrap();
    }
}
