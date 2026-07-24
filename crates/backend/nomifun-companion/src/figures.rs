//! Decoupled custom-figure **library**: figures live independently of any companion,
//! so a user can create/import a figure up-front (from the 电子伙伴 home page)
//! before a companion exists, reuse one figure across several companions, and pick a saved
//! figure when creating/editing a companion.
//!
//! Storage (shared, under the backend data dir):
//!   `{figures_dir}/{figure_id}.webp`  — the processed cutout image bytes
//!   `{figures_dir}/index.json`        — `{ figures: [FigureMeta, …] }`
//!
//! Ingest reuses [`crate::figure::validate_figure_source`] (same sandbox +
//! magic + size + dimension checks as the per-companion path). Index read-modify-write
//! is serialized by the caller ([`crate::service::CompanionService`] holds the lock),
//! so these functions stay pure over `figures_dir`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nomifun_common::{AppError, FigureId, now_ms};
use serde::{Deserialize, Serialize};

use crate::profile::HeadBox;

const INDEX_FILE: &str = "index.json";
/// Cap on a figure's display name (chars). Generous; just stops abuse.
const MAX_NAME_CHARS: usize = 40;

/// One library figure. Mirrors `FigureMeta` in the UI (`characters/types.ts`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FigureMeta {
    /// Stable cross-device business ID: canonical lowercase bare UUIDv7.
    pub figure_id: String,
    /// User-facing label.
    pub name: String,
    /// width / height of the cutout image.
    pub aspect: f32,
    pub head_box: HeadBox,
    /// Desk size tier: "s" | "m" | "l".
    pub size_tier: String,
    /// Creation time, unix milliseconds.
    pub created_at: i64,
}

/// Editable library-figure metadata. Image bytes, `figure_id`, aspect and
/// `created_at` stay immutable.
#[derive(Debug, Clone, Default)]
pub struct FigureUpdate {
    pub name: Option<String>,
    pub head_box: Option<HeadBox>,
    pub size_tier: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FigureIndex {
    figures: Vec<FigureMeta>,
}

fn image_name(figure_id: &str) -> String {
    format!("{figure_id}.webp")
}

/// Reject IDs that could escape `figures_dir` (path separators / traversal) or
/// are not canonical UUIDv7 values. `read`/`delete` take `figure_id` from a URL
/// path param, so this is the trust boundary.
fn is_safe_id(figure_id: &str) -> bool {
    FigureId::parse(figure_id).is_ok()
}

fn sanitize_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let name: String = trimmed.chars().take(MAX_NAME_CHARS).collect();
    if name.is_empty() { "自定义形象".to_owned() } else { name }
}

fn normalize_tier(tier: &str) -> String {
    match tier {
        "s" | "l" => tier.to_owned(),
        _ => "m".to_owned(),
    }
}

fn validate_aspect(aspect: f32) -> Result<(), AppError> {
    if !aspect.is_finite() || aspect <= 0.0 {
        return Err(AppError::BadRequest(
            "figure aspect must be finite and greater than zero".into(),
        ));
    }
    Ok(())
}

fn validate_head_box(head_box: &HeadBox) -> Result<(), AppError> {
    let values = [head_box.x, head_box.y, head_box.w, head_box.h];
    if values.iter().any(|value| !value.is_finite()) {
        return Err(AppError::BadRequest(
            "figure head_box values must be finite".into(),
        ));
    }
    if head_box.x < 0.0
        || head_box.y < 0.0
        || head_box.w <= 0.0
        || head_box.h < 0.0
        || head_box.x + head_box.w > 1.0
        || head_box.y + head_box.h > 1.0
    {
        return Err(AppError::BadRequest(
            "figure head_box must fit inside normalized image bounds".into(),
        ));
    }
    Ok(())
}

fn inventory_error(figures_dir: &Path, detail: impl std::fmt::Display) -> AppError {
    AppError::Internal(format!(
        "figure library inventory {} is inconsistent: {detail}",
        figures_dir.display()
    ))
}

/// Cross-check the index against every durable image/tombstone. v3 has one
/// authoritative representation: an indexed image must exist as a regular
/// file, and an unindexed `.webp`/delete tombstone is a hard startup/read error
/// that requires an explicit reset or repair.
fn validate_inventory(figures_dir: &Path, index: &FigureIndex) -> Result<(), AppError> {
    let indexed: HashSet<&str> = index.figures.iter().map(|figure| figure.figure_id.as_str()).collect();
    let entries = match std::fs::read_dir(figures_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if indexed.is_empty() {
                return Ok(());
            }
            return Err(inventory_error(figures_dir, "directory is missing but index is non-empty"));
        }
        Err(error) => return Err(inventory_error(figures_dir, error)),
    };
    let mut images = HashMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| inventory_error(figures_dir, error))?;
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| inventory_error(figures_dir, "contains a non-UTF8 entry name"))?;
        if file_name == INDEX_FILE {
            continue;
        }
        if file_name.starts_with('.') && file_name.ends_with(".webp.delete") {
            return Err(inventory_error(
                figures_dir,
                format!("contains stale deletion tombstone {file_name:?}"),
            ));
        }
        let Some(id) = file_name.strip_suffix(".webp") else {
            continue;
        };
        FigureId::parse(id).map_err(|error| {
            inventory_error(
                figures_dir,
                format!("contains non-canonical image file {file_name:?}: {error}"),
            )
        })?;
        let file_type = entry
            .file_type()
            .map_err(|error| inventory_error(figures_dir, error))?;
        if !file_type.is_file() {
            return Err(inventory_error(
                figures_dir,
                format!("image {file_name:?} is not a regular file"),
            ));
        }
        images.insert(id.to_owned(), entry.path());
    }
    for id in &indexed {
        if !images.contains_key(*id) {
            return Err(inventory_error(
                figures_dir,
                format!("indexed image {id:?} is missing"),
            ));
        }
    }
    for id in images.keys() {
        if !indexed.contains(id.as_str()) {
            return Err(inventory_error(
                figures_dir,
                format!("contains orphaned image {id:?}"),
            ));
        }
    }
    Ok(())
}

fn load_index(figures_dir: &Path) -> Result<FigureIndex, AppError> {
    let path = figures_dir.join(INDEX_FILE);
    let index: FigureIndex = crate::fsio::load_json_optional(&path)
        .map_err(|error| AppError::Internal(format!("load figure index {}: {error}", path.display())))?
        .unwrap_or_default();
    let mut ids = std::collections::HashSet::new();
    for figure in &index.figures {
        FigureId::parse(&figure.figure_id).map_err(|error| {
            AppError::Internal(format!(
                "figure index {} contains non-canonical id {:?}: {error}",
                path.display(),
                figure.figure_id
            ))
        })?;
        if !ids.insert(figure.figure_id.as_str()) {
            return Err(AppError::Internal(format!(
                "figure index {} contains duplicate id '{}'",
                path.display(),
                figure.figure_id
            )));
        }
        validate_aspect(figure.aspect).map_err(|error| {
            AppError::Internal(format!(
                "figure '{}' has invalid aspect in {}: {error}",
                figure.figure_id,
                path.display()
            ))
        })?;
        validate_head_box(&figure.head_box).map_err(|error| {
            AppError::Internal(format!(
                "figure '{}' has invalid head_box in {}: {error}",
                figure.figure_id,
                path.display()
            ))
        })?;
        if !matches!(figure.size_tier.as_str(), "s" | "m" | "l") {
            return Err(AppError::Internal(format!(
                "figure '{}' has invalid size_tier in {}",
                figure.figure_id,
                path.display()
            )));
        }
    }
    validate_inventory(figures_dir, &index)?;
    Ok(index)
}

fn save_index(figures_dir: &Path, index: &FigureIndex) -> Result<(), AppError> {
    crate::fsio::save_json_atomic(figures_dir, INDEX_FILE, index)
        .map_err(|e| AppError::Internal(format!("save figure index: {e}")))
}

/// Boot-time integrity audit for the whole library.
pub(crate) fn validate_store(figures_dir: &Path) -> Result<(), AppError> {
    load_index(figures_dir).map(|_| ())
}

/// Canonical IDs currently present in the fully-audited library.
pub(crate) fn id_set(figures_dir: &Path) -> Result<HashSet<String>, AppError> {
    Ok(load_index(figures_dir)?
        .figures
        .into_iter()
        .map(|figure| figure.figure_id)
        .collect())
}

/// All saved figures, newest first.
pub fn list(figures_dir: &Path) -> Result<Vec<FigureMeta>, AppError> {
    let mut figures = load_index(figures_dir)?.figures;
    figures.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(figures)
}

/// Ingest a validated upload as a new library figure; returns its metadata.
pub fn create(
    figures_dir: &Path,
    source_path: &Path,
    name: &str,
    aspect: f32,
    head_box: HeadBox,
    size_tier: &str,
) -> Result<FigureMeta, AppError> {
    validate_aspect(aspect)?;
    validate_head_box(&head_box)?;
    let bytes = crate::figure::validate_figure_source(source_path)?;
    let mut index = load_index(figures_dir)?;
    let figure_id = FigureId::new().into_string();
    let image = figures_dir.join(image_name(&figure_id));
    crate::fsio::save_bytes_atomic(figures_dir, &image_name(&figure_id), &bytes)
        .map_err(|e| AppError::Internal(format!("save library figure: {e}")))?;

    let meta = FigureMeta {
        figure_id: figure_id.clone(),
        name: sanitize_name(name),
        aspect,
        head_box,
        size_tier: normalize_tier(size_tier),
        created_at: now_ms(),
    };
    index.figures.push(meta.clone());
    if let Err(error) = save_index(figures_dir, &index) {
        if let Err(cleanup_error) = crate::fsio::remove_path_entry(&image) {
            tracing::error!(
                %cleanup_error,
                path = %image.display(),
                "failed to roll back orphaned figure image after index save failure"
            );
        }
        return Err(error);
    }
    Ok(meta)
}

/// One indexed figure's image bytes + mtime (unix seconds, the ETag input).
pub fn read_image(
    figures_dir: &Path,
    figure_id: &str,
) -> Result<Option<(Vec<u8>, u64)>, AppError> {
    if !is_safe_id(figure_id) {
        return Ok(None);
    }
    if !load_index(figures_dir)?
        .figures
        .iter()
        .any(|figure| figure.figure_id == figure_id)
    {
        return Ok(None);
    }
    let path = figures_dir.join(image_name(figure_id));
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::Internal(format!(
                "figure '{}' is indexed but its image is missing",
                figure_id
            )));
        }
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read figure image metadata {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.is_file() {
        return Err(AppError::Internal(format!(
            "figure image is not a regular file: {}",
            path.display()
        )));
    }
    let mtime = metadata
        .modified()
        .map_err(|error| AppError::Internal(format!("read figure image mtime: {error}")))?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| AppError::Internal(format!("figure image mtime predates epoch: {error}")))?
        .as_secs();
    let bytes = std::fs::read(&path)
        .map_err(|error| AppError::Internal(format!("read figure image {}: {error}", path.display())))?;
    crate::figure::validate_figure_bytes(&bytes)?;
    Ok(Some((bytes, mtime)))
}

/// Rename a figure. Unknown `figure_id` → 404.
pub fn rename(
    figures_dir: &Path,
    figure_id: &str,
    name: &str,
) -> Result<FigureMeta, AppError> {
    update(
        figures_dir,
        figure_id,
        FigureUpdate {
            name: Some(name.to_owned()),
            head_box: None,
            size_tier: None,
        },
    )
}

/// Update editable figure metadata. Unknown `figure_id` → 404.
pub fn update(
    figures_dir: &Path,
    figure_id: &str,
    patch: FigureUpdate,
) -> Result<FigureMeta, AppError> {
    if !is_safe_id(figure_id) {
        return Err(AppError::NotFound(format!(
            "figure '{figure_id}' not found"
        )));
    }
    let mut index = load_index(figures_dir)?;
    let entry = index
        .figures
        .iter_mut()
        .find(|figure| figure.figure_id == figure_id)
        .ok_or_else(|| AppError::NotFound(format!("figure '{figure_id}' not found")))?;
    if let Some(name) = patch.name {
        entry.name = sanitize_name(&name);
    }
    if let Some(head_box) = patch.head_box {
        validate_head_box(&head_box)?;
        entry.head_box = head_box;
    }
    if let Some(size_tier) = patch.size_tier {
        entry.size_tier = normalize_tier(&size_tier);
    }
    let updated = entry.clone();
    save_index(figures_dir, &index)?;
    Ok(updated)
}

/// Delete a figure (image + index entry). The preflight inventory audit requires
/// both representations to be intact; unknown IDs return 404 and a missing image
/// fails closed instead of silently dropping only the index entry.
pub fn remove(figures_dir: &Path, figure_id: &str) -> Result<(), AppError> {
    if !is_safe_id(figure_id) {
        return Err(AppError::NotFound(format!(
            "figure '{figure_id}' not found"
        )));
    }
    let mut index = load_index(figures_dir)?;
    let before = index.figures.len();
    index.figures.retain(|figure| figure.figure_id != figure_id);
    if index.figures.len() == before {
        return Err(AppError::NotFound(format!(
            "figure '{figure_id}' not found"
        )));
    }
    let image = figures_dir.join(image_name(figure_id));
    let tombstone = figures_dir.join(format!(".{figure_id}.webp.delete"));
    let image_staged = match std::fs::rename(&image, &tombstone) {
        Ok(()) => {
            crate::fsio::sync_dir(figures_dir).map_err(|error| {
                AppError::Internal(format!(
                    "fsync figure directory after staging deletion: {error}"
                ))
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(AppError::Internal(format!(
                "stage figure image deletion {}: {error}",
                image.display()
            )));
        }
    };
    if let Err(error) = save_index(figures_dir, &index) {
        if image_staged
            && let Err(restore_error) = std::fs::rename(&tombstone, &image)
        {
            return Err(AppError::Internal(format!(
                "{error}; additionally failed to restore figure image after index rollback: {restore_error}"
            )));
        }
        if image_staged {
            crate::fsio::sync_dir(figures_dir).map_err(|sync_error| {
                AppError::Internal(format!(
                    "{error}; restored figure image but failed to fsync directory: {sync_error}"
                ))
            })?;
        }
        return Err(error);
    }
    if image_staged {
        crate::fsio::remove_path_entry(&tombstone).map_err(|error| {
            AppError::Internal(format!(
                "figure index deletion committed but tombstone cleanup failed: {error}"
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload_scratch() -> tempfile::TempDir {
        let root = std::env::temp_dir().join("nomifun");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new().prefix("figlib-test-").tempdir_in(root).unwrap()
    }

    /// A real 7×5 lossless WebP (VP8L), same bytes the figure.rs tests use.
    fn webp_bytes() -> Vec<u8> {
        vec![
            0x52, 0x49, 0x46, 0x46, 0x1E, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x4C, 0x11, 0x00, 0x00, 0x00, 0x2F, 0x06, 0x00, 0x01, 0x00, 0x07, 0x50, 0x8A,
            0x2A, 0xD4, 0xA3, 0xFF, 0x81, 0x88, 0xE8, 0x7F, 0x00, 0x00,
        ]
    }

    fn make_source(upload: &tempfile::TempDir, file: &str) -> std::path::PathBuf {
        let p = upload.path().join(file);
        std::fs::write(&p, webp_bytes()).unwrap();
        p
    }

    #[test]
    fn create_list_read_rename_delete_roundtrip() {
        let upload = upload_scratch();
        let figs = tempfile::tempdir().unwrap();
        let dir = figs.path();

        let hb = HeadBox { x: 0.3, y: 0.0, w: 0.4, h: 0.4 };
        let a = create(dir, &make_source(&upload, "a.webp"), "阿狸", 0.7, hb.clone(), "l").unwrap();
        let b = create(dir, &make_source(&upload, "b.webp"), "", 1.0, hb.clone(), "bogus").unwrap();

        assert!(FigureId::parse(&a.figure_id).is_ok());
        assert_eq!(a.name, "阿狸");
        assert_eq!(a.size_tier, "l");
        assert_eq!(b.name, "自定义形象"); // empty → default
        assert_eq!(b.size_tier, "m"); // bogus tier → m

        // newest first
        let listed = list(dir).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|f| f.figure_id == a.figure_id));
        assert!(listed.iter().any(|f| f.figure_id == b.figure_id));
        if b.created_at > a.created_at {
            assert_eq!(listed[0].figure_id, b.figure_id);
        }

        // image readable
        let (bytes, _) = read_image(dir, &a.figure_id).unwrap().unwrap();
        assert_eq!(bytes, webp_bytes());

        // rename
        let renamed = rename(dir, &a.figure_id, "新名字").unwrap();
        assert_eq!(renamed.name, "新名字");
        assert_eq!(list(dir).unwrap().iter().find(|f| f.figure_id == a.figure_id).unwrap().name, "新名字");

        // update editable framing metadata without touching immutable image/aspect.
        let updated_head = HeadBox { x: 0.1, y: 0.2, w: 0.5, h: 0.6 };
        let updated = update(
            dir,
            &a.figure_id,
            FigureUpdate { name: Some("新取景".to_owned()), head_box: Some(updated_head.clone()), size_tier: Some("s".to_owned()) },
        )
        .unwrap();
        assert_eq!(updated.name, "新取景");
        assert_eq!(updated.aspect, a.aspect);
        assert_eq!(updated.created_at, a.created_at);
        assert_eq!(updated.head_box, updated_head);
        assert_eq!(updated.size_tier, "s");
        assert_eq!(list(dir).unwrap().iter().find(|f| f.figure_id == a.figure_id).unwrap().head_box, updated_head);

        // delete drops index + image
        remove(dir, &a.figure_id).unwrap();
        assert_eq!(list(dir).unwrap().len(), 1);
        assert!(read_image(dir, &a.figure_id).unwrap().is_none());
        assert!(remove(dir, &a.figure_id).is_err()); // already gone → 404
    }

    #[test]
    fn rejects_unsafe_ids() {
        let figs = tempfile::tempdir().unwrap();
        assert!(read_image(figs.path(), "../escape").unwrap().is_none());
        assert!(read_image(figs.path(), "id_../x").unwrap().is_none());
        assert!(read_image(figs.path(), "notaprefix").unwrap().is_none());
        assert!(
            read_image(figs.path(), "id_550e8400-e29b-41d4-a716-446655440000").unwrap().is_none(),
            "parseable non-v7 UUIDs are not canonical figure IDs"
        );
        assert!(rename(figs.path(), "../x", "n").is_err());
        assert!(update(figs.path(), "../x", FigureUpdate { name: Some("n".into()), head_box: None, size_tier: None }).is_err());
        assert!(remove(figs.path(), "id_a/b").is_err());
    }

    #[test]
    fn inventory_fails_closed_on_orphan_missing_image_or_tombstone() {
        let upload = upload_scratch();

        let orphaned = tempfile::tempdir().unwrap();
        let orphan_id = FigureId::new().into_string();
        std::fs::write(orphaned.path().join(image_name(&orphan_id)), webp_bytes()).unwrap();
        assert!(list(orphaned.path()).unwrap_err().to_string().contains("orphaned image"));

        let missing = tempfile::tempdir().unwrap();
        let created = create(
            missing.path(),
            &make_source(&upload, "missing.webp"),
            "missing",
            1.0,
            HeadBox { x: 0.1, y: 0.1, w: 0.5, h: 0.5 },
            "m",
        )
        .unwrap();
        std::fs::remove_file(missing.path().join(image_name(&created.figure_id))).unwrap();
        assert!(list(missing.path()).unwrap_err().to_string().contains("is missing"));

        let tombstoned = tempfile::tempdir().unwrap();
        let tombstone_id = FigureId::new().into_string();
        std::fs::write(
            tombstoned.path().join(format!(".{tombstone_id}.webp.delete")),
            webp_bytes(),
        )
        .unwrap();
        assert!(list(tombstoned.path()).unwrap_err().to_string().contains("tombstone"));
    }
}
