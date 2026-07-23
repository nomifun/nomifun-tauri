use std::path::{Path, PathBuf};
use std::sync::Arc;

use nomifun_api_types::{AttachmentDto, NewAttachmentRef};
use nomifun_common::{AppError, AttachmentId, RequirementId, generate_id, now_ms};
use nomifun_db::IAttachmentRepository;
use nomifun_db::models::AttachmentRow;
use nomifun_file::path_safety::{has_traversal, validate_path};
use tracing::warn;

/// Upload whitelist —images only this iteration, aligned with the frontend
/// `imageExts` (FileService.ts).
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "svg"];

/// Directory (relative to the data dir) where attachment originals live:
/// `attachments/{requirement_id}/{att_id}.{ext}`. The former generic
/// `{kind}/{target_id}` polymorphism collapsed to a single requirement domain.
const ATTACHMENTS_REL_DIR: &str = "attachments";

/// Directory (relative to a session workspace) where AutoWork stages copies
/// for the model to read: `.nomi/requirement-attachments/{req_id}/{file_name}`.
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
}

impl AttachmentStore {
    pub fn new(data_dir: PathBuf, repo: Arc<dyn IAttachmentRepository>) -> Self {
        Self {
            data_dir,
            upload_root: std::env::temp_dir().join("nomifun"),
            repo,
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
        self.data_dir.join(&row.rel_path)
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

    pub async fn list(&self, requirement_id: &str) -> Result<Vec<AttachmentRow>, AppError> {
        validate_requirement_id(requirement_id)?;
        Ok(self.repo.list_for_requirement(requirement_id).await?)
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
        for attachment_id in attachment_ids {
            let Some(row) = self.repo.get_by_attachment_id(attachment_id).await? else {
                continue;
            };
            if row.requirement_id != requirement_id {
                warn!(
                    attachment_id,
                    "attachment remove skipped: requirement mismatch"
                );
                continue;
            }
            let abs = self.abs_path(&row);
            let staged = self.stage_file_for_delete(&abs).await?;
            match self.repo.delete(row.id).await {
                Ok(_) => {
                    self.finish_staged_delete(staged).await;
                }
                Err(error) => {
                    self.restore_staged_file(staged).await?;
                    return Err(error.into());
                }
            };
        }
        Ok(())
    }

    /// Delete every attachment of a requirement (rows + files + dir).
    ///
    /// Files are first renamed into a same-filesystem quarantine. Only after
    /// every file is safely staged are all database rows deleted. A DB failure
    /// restores the original paths; a filesystem staging failure leaves the DB
    /// untouched. This is the strongest atomicity available across SQLite and
    /// the filesystem without a distributed transaction manager.
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
        let rows = self.repo.list_for_requirement(requirement_id).await?;
        let mut staged_files = Vec::with_capacity(rows.len());
        for row in &rows {
            let abs = self.abs_path(row);
            match self.stage_file_for_delete(&abs).await {
                Ok(staged) => staged_files.push(staged),
                Err(error) => {
                    for staged in staged_files.into_iter().rev() {
                        if let Err(restore_error) = self.restore_staged_file(staged).await {
                            warn!(error = %restore_error, "attachment rollback failed after staging error");
                        }
                    }
                    return Err(error);
                }
            }
        }
        Ok(PreparedAttachmentDelete {
            requirement_dir: self.requirement_dir(requirement_id),
            staged_files,
        })
    }

    pub(crate) async fn restore_prepared_delete(
        &self,
        prepared: PreparedAttachmentDelete,
    ) -> Result<(), AppError> {
        for staged in prepared.staged_files.into_iter().rev() {
            self.restore_staged_file(staged).await?;
        }
        Ok(())
    }

    pub(crate) async fn finish_prepared_delete(&self, prepared: PreparedAttachmentDelete) {
        for staged in prepared.staged_files {
            self.finish_staged_delete(staged).await;
        }
        if let Err(error) = remove_dir_if_empty(&prepared.requirement_dir).await {
            warn!(error = %error, path = %prepared.requirement_dir.display(), "attachment directory cleanup failed after committed delete");
        }
    }

    async fn stage_file_for_delete(&self, original: &Path) -> Result<Option<StagedFileDelete>, AppError> {
        match tokio::fs::symlink_metadata(original).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(AppError::Internal(format!(
                    "attachment path is not a regular file: {}",
                    original.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "inspect attachment file '{}' failed: {error}",
                    original.display()
                )));
            }
        }

        let file_name = original
            .file_name()
            .ok_or_else(|| AppError::Internal(format!(
                "attachment path has no file name: {}",
                original.display()
            )))?
            .to_string_lossy();
        let staged = original.with_file_name(format!(".{file_name}.deleting-{}", generate_id()));
        tokio::fs::rename(original, &staged).await.map_err(|error| {
            AppError::Internal(format!(
                "stage attachment file '{}' for deletion failed: {error}",
                original.display()
            ))
        })?;
        Ok(Some(StagedFileDelete {
            original: original.to_path_buf(),
            staged,
        }))
    }

    async fn restore_staged_file(&self, staged: Option<StagedFileDelete>) -> Result<(), AppError> {
        let Some(staged) = staged else {
            return Ok(());
        };
        tokio::fs::rename(&staged.staged, &staged.original)
            .await
            .map_err(|error| {
                AppError::Internal(format!(
                    "restore attachment file '{}' failed: {error}",
                    staged.original.display()
                ))
            })
    }

    async fn finish_staged_delete(&self, staged: Option<StagedFileDelete>) {
        let Some(staged) = staged else {
            return;
        };
        if let Err(error) = tokio::fs::remove_file(&staged.staged).await {
            warn!(
                error = %error,
                path = %staged.staged.display(),
                "staged attachment file cleanup failed after committed database delete"
            );
        }
    }

    /// Build prompt entries for a requirement's attachments, copying each into
    /// `{workspace}/.nomi/requirement-attachments/{req_id}/{file_name}` when a
    /// workspace is given. Best-effort and infallible: a failed copy falls back
    /// to the absolute original path; a vanished original is flagged `missing`.
    pub async fn stage_for_prompt(
        &self,
        req_id: &str,
        workspace: Option<&Path>,
    ) -> Vec<PromptAttachment> {
        if validate_requirement_id(req_id).is_err() {
            warn!(req_id, "refusing to stage attachments for an invalid requirement id");
            return Vec::new();
        }
        let rows = match self.repo.list_for_requirement(req_id).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, req_id, "failed to list attachments for staging");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let orig = self.abs_path(row);
            if !orig.exists() {
                warn!(req_id, file = %row.file_name, "attachment original missing");
                out.push(PromptAttachment {
                    file_name: row.file_name.clone(),
                    path: String::new(),
                    missing: true,
                });
                continue;
            }
            let mut path = orig.to_string_lossy().to_string();
            if let Some(ws) = workspace.filter(|w| !w.as_os_str().is_empty()) {
                let stage_root = ws.join(WORKSPACE_STAGE_REL_DIR);
                let dest_dir = stage_root.join(req_id.to_string());
                let staged: Result<(), std::io::Error> = async {
                    tokio::fs::create_dir_all(&dest_dir).await?;
                    // Self-isolate like the knowledge mounts: never pollute the
                    // workspace's VCS status.
                    let gitignore = stage_root.join(".gitignore");
                    if !gitignore.exists() {
                        let _ = tokio::fs::write(&gitignore, "*\n").await;
                    }
                    tokio::fs::copy(&orig, dest_dir.join(&row.file_name)).await?;
                    Ok(())
                }
                .await;
                match staged {
                    Ok(()) => {
                        path = format!("./{WORKSPACE_STAGE_REL_DIR}/{req_id}/{}", row.file_name);
                    }
                    Err(e) => {
                        warn!(error = %e, req_id, file = %row.file_name, "workspace staging failed —falling back to absolute path");
                    }
                }
            }
            out.push(PromptAttachment {
                file_name: row.file_name.clone(),
                path,
                missing: false,
            });
        }
        out
    }
}

#[derive(Debug)]
struct StagedFileDelete {
    original: PathBuf,
    staged: PathBuf,
}

pub(crate) struct PreparedAttachmentDelete {
    requirement_dir: PathBuf,
    staged_files: Vec<Option<StagedFileDelete>>,
}

async fn remove_dir_if_empty(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Internal(format!(
            "remove empty attachment directory '{}' failed: {error}",
            path.display()
        ))),
    }
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
    use nomifun_db::{SqliteAttachmentRepository, init_database_memory};
    use std::sync::Arc;

    const REQ_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const REQ_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";

    async fn store() -> (AttachmentStore, tempfile::TempDir, tempfile::TempDir) {
        let db = init_database_memory().await.unwrap();
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
        let repo: Arc<dyn nomifun_db::IAttachmentRepository> =
            Arc::new(SqliteAttachmentRepository::new(db.pool().clone()));
        Box::leak(Box::new(db));
        let data_dir = tempfile::tempdir().unwrap();
        let upload_root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(data_dir.path().to_path_buf(), repo)
            .with_upload_root(upload_root.path().to_path_buf());
        (store, data_dir, upload_root)
    }

    fn put_upload(root: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        let p = root.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
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
    async fn stage_for_prompt_copies_into_workspace_and_falls_back() {
        let (store, data_dir, upload_root) = store().await;
        let a = put_upload(upload_root.path(), "a.png", b"x");
        let rows = store
            .ingest(REQ_1, &[NewAttachmentRef { source_path: a, file_name: "图片.png".into() }], None)
            .await
            .unwrap();
        // workspace staging → relative path + copy exists + .gitignore written
        let ws = tempfile::tempdir().unwrap();
        let staged = store.stage_for_prompt(REQ_1, Some(ws.path())).await;
        assert_eq!(staged.len(), 1);
        assert!(!staged[0].missing);
        if staged[0].path.starts_with("./.nomi/requirement-attachments/") {
            assert!(
                ws.path()
                    .join(staged[0].path.trim_start_matches("./"))
                    .exists()
            );
        } else {
            // Some platforms/filesystems reject the deliberately non-ASCII
            // fixture name; staging is best-effort and must then fall back to
            // the persisted absolute original.
            assert_eq!(
                staged[0].path,
                data_dir
                    .path()
                    .join(&rows[0].rel_path)
                    .to_string_lossy()
                    .to_string()
            );
        }
        assert!(ws.path().join(".nomi/requirement-attachments/.gitignore").exists());
        // no workspace → absolute original path
        let staged = store.stage_for_prompt(REQ_1, None).await;
        assert_eq!(staged[0].path, data_dir.path().join(&rows[0].rel_path).to_string_lossy().to_string());
        // original deleted → missing flag
        std::fs::remove_file(data_dir.path().join(&rows[0].rel_path)).unwrap();
        let staged = store.stage_for_prompt(REQ_1, Some(ws.path())).await;
        assert!(staged[0].missing);
    }
}
