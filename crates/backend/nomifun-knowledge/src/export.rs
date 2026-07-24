//! Knowledge base zip export/import (spec §4.8) — cross-machine migration.
//!
//! Package layout (zip root):
//! - `manifest.json` — `{format, version, kind, exported_at, app_version}`
//!   envelope, validated on import (wrong format/kind or a newer version is
//!   rejected before anything touches the registry).
//! - `meta.json` — `{name, description}` of the exported base.
//! - `files/**` — every `.md` under the base root with relative paths
//!   preserved. `_inbox/` is included on purpose: staged write-backs are
//!   user data and must survive a machine migration.
//!
//! Import extracts into a temp dir under the managed knowledge dir (same
//! volume as the final destination, so file moves are cheap renames), with
//! zip-slip hardening mirroring `nomifun-extension`'s skill import: entry
//! paths are component-sanitized, symlink entries are rejected, and only
//! `manifest.json` / `meta.json` / `files/**.md` entries are accepted.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use nomifun_common::{AppError, KnowledgeBaseId, TimestampMs, now_ms};
use serde::{Deserialize, Serialize};

use crate::KB_MANAGED_REL_DIR;
use crate::service::{KnowledgeService, is_md};

/// `manifest.json` envelope discriminators. `version` is bumped only on
/// breaking package-layout changes; readers accept anything `<= EXPORT_VERSION`.
pub const EXPORT_FORMAT: &str = "nomifun-export";
pub const EXPORT_KIND: &str = "knowledge-base";
pub const EXPORT_VERSION: u32 = 1;

/// Result of a successful export, returned to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    pub file_count: u64,
    /// Uncompressed size of the packaged `.md` files.
    pub total_bytes: u64,
    pub dest_path: String,
}

/// Result of a successful import, returned to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ImportSummary {
    pub kb_id: KnowledgeBaseId,
    /// Final name after duplicate-name suffixing (`"name (2)"`, …).
    pub name: String,
    pub file_count: u64,
}

#[derive(Debug, Serialize)]
struct ExportManifest {
    format: String,
    version: u32,
    kind: String,
    exported_at: TimestampMs,
    app_version: String,
}

/// `meta.json` payload. Lenient on read: missing fields default to empty so
/// a hand-edited package still imports.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ExportMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
}

// ── Export ──────────────────────────────────────────────────────────

/// Package the base `kb_id` into a zip at `dest_path` (written atomically
/// via `{dest}.tmp` + rename).
pub async fn export_base(
    service: &KnowledgeService,
    kb_id: &str,
    dest_path: &Path,
) -> Result<ExportSummary, AppError> {
    let info = service.get_base_info(kb_id).await?;
    if !info.root_exists {
        return Err(AppError::BadRequest(format!(
            "knowledge base directory missing: {}",
            info.root_path
        )));
    }
    if !dest_path.is_absolute() {
        return Err(AppError::BadRequest("dest_path must be absolute".into()));
    }

    let root = PathBuf::from(&info.root_path);
    let meta = ExportMeta {
        name: info.name,
        description: info.description,
    };
    let dest = dest_path.to_path_buf();
    let (file_count, total_bytes) = tokio::task::spawn_blocking(move || build_zip(&root, &meta, &dest))
        .await
        .map_err(|e| AppError::Internal(format!("export task join error: {e}")))??;

    Ok(ExportSummary {
        file_count,
        total_bytes,
        dest_path: dest_path.to_string_lossy().to_string(),
    })
}

/// Blocking core of the export: walk `root` for `.md` files and write the
/// package to `dest` via a `.tmp` sibling. Returns `(file_count, total_bytes)`.
fn build_zip(root: &Path, meta: &ExportMeta, dest: &Path) -> Result<(u64, u64), AppError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Internal(format!("failed to create export dir: {e}")))?;
    }
    let mut tmp_name = dest.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let counts = match write_zip_to(root, meta, &tmp) {
        Ok(counts) => counts,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AppError::Internal(format!("failed to finalize export file: {e}")));
    }
    Ok(counts)
}

fn write_zip_to(root: &Path, meta: &ExportMeta, tmp: &Path) -> Result<(u64, u64), AppError> {
    let io_err = |what: &str| {
        let what = what.to_owned();
        move |e: std::io::Error| AppError::Internal(format!("{what}: {e}"))
    };
    let zip_err = |e: zip::result::ZipError| AppError::Internal(format!("failed to write zip: {e}"));

    let file = std::fs::File::create(tmp).map_err(io_err("failed to create export file"))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    let manifest = ExportManifest {
        format: EXPORT_FORMAT.to_owned(),
        version: EXPORT_VERSION,
        kind: EXPORT_KIND.to_owned(),
        exported_at: now_ms(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    zip.start_file("manifest.json", options).map_err(zip_err)?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest).map_err(|e| AppError::Internal(e.to_string()))?)
        .map_err(io_err("failed to write manifest"))?;
    zip.start_file("meta.json", options).map_err(zip_err)?;
    zip.write_all(&serde_json::to_vec_pretty(meta).map_err(|e| AppError::Internal(e.to_string()))?)
        .map_err(io_err("failed to write meta"))?;

    // Sorted relative paths → deterministic packages (friendlier diffing).
    let mut rels: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && is_md(e.path()))
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    rels.sort();

    let mut file_count = 0u64;
    let mut total_bytes = 0u64;
    for rel in rels {
        let bytes = std::fs::read(root.join(&rel)).map_err(io_err(&format!("failed to read {rel}")))?;
        zip.start_file(format!("files/{rel}"), options).map_err(zip_err)?;
        zip.write_all(&bytes).map_err(io_err(&format!("failed to package {rel}")))?;
        file_count += 1;
        total_bytes += bytes.len() as u64;
    }

    zip.finish().map_err(zip_err)?;
    Ok((file_count, total_bytes))
}

// ── Import ──────────────────────────────────────────────────────────

/// Import a package created by [`export_base`]: validate, create a new
/// managed base (name deduplicated against existing bases), and move the
/// packaged files into its root. Emits `knowledge.base-created` via the
/// service's create path, then `knowledge.base-updated` once files landed
/// so clients see correct stats.
pub async fn import_base(service: &KnowledgeService, src_path: &Path) -> Result<ImportSummary, AppError> {
    if !src_path.is_file() {
        return Err(AppError::BadRequest(format!(
            "import file does not exist: {}",
            src_path.display()
        )));
    }

    // Extraction temp lives next to the managed bases (same volume → the
    // final move is a cheap rename), namespaced to avoid collisions.
    let tmp_root = service.data_dir().join(KB_MANAGED_REL_DIR).join(".import-tmp");
    let extract_dir = tmp_root.join(format!("kb-{}-{}", std::process::id(), now_ms()));
    tokio::fs::create_dir_all(&extract_dir)
        .await
        .map_err(|e| AppError::Internal(format!("failed to create import temp dir: {e}")))?;

    let result = import_extracted(service, src_path, &extract_dir).await;
    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
    let _ = tokio::fs::remove_dir(&tmp_root).await; // best-effort, only when empty
    result
}

async fn import_extracted(
    service: &KnowledgeService,
    src_path: &Path,
    extract_dir: &Path,
) -> Result<ImportSummary, AppError> {
    let src = src_path.to_path_buf();
    let dest = extract_dir.to_path_buf();
    let meta = tokio::task::spawn_blocking(move || extract_zip_validated(&src, &dest))
        .await
        .map_err(|e| AppError::Internal(format!("import task join error: {e}")))??;

    let existing: HashSet<String> = service
        .list_bases()
        .await?
        .into_iter()
        .map(|info| info.name)
        .collect();
    let base_name = match meta.name.trim() {
        "" => "导入的知识库",
        name => name,
    };
    let final_name = dedup_name(&existing, base_name);

    // Existing managed-create path: provisions `{data_dir}/knowledge/{id}`
    // and emits `knowledge.base-created`. (Imported packages carry no URL
    // source — `extra` starts empty.)
    let info = service.create_base(&final_name, &meta.description, None, None).await?;

    let files_src = extract_dir.join("files");
    let files_dest = PathBuf::from(&info.root_path);
    let moved = tokio::task::spawn_blocking(move || move_file_tree(&files_src, &files_dest))
        .await
        .map_err(|e| AppError::Internal(format!("import move task join error: {e}")));
    let file_count = match moved {
        Ok(Ok(count)) => count,
        Ok(Err(e)) | Err(e) => {
            // Roll back the half-created base (purge is safe: managed dir).
            if let Err(del) = service.delete_base(&info.knowledge_base_id, true).await {
                tracing::warn!(kb_id = %info.knowledge_base_id, error = %del, "rollback of failed import left a stale base");
            }
            return Err(e);
        }
    };

    // Re-emit with fresh file stats so clients don't show a 0-file base.
    if let Err(e) = service.update_base(&info.knowledge_base_id, None, None, None).await {
        tracing::warn!(kb_id = %info.knowledge_base_id, error = %e, "failed to refresh base info after import");
    }

    Ok(ImportSummary {
        kb_id: info.knowledge_base_id,
        name: final_name,
        file_count,
    })
}

/// Blocking extraction with validation. Only `manifest.json`, `meta.json`
/// and `files/**.md` entries are accepted; every entry path is sanitized
/// (zip-slip) and symlink entries are rejected. Returns the parsed meta
/// after the manifest passed format/kind/version checks.
fn extract_zip_validated(archive_path: &Path, destination: &Path) -> Result<ExportMeta, AppError> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| AppError::BadRequest(format!("failed to open import file: {e}")))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|_| AppError::BadRequest("不是知识库导出包".into()))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| AppError::BadRequest(format!("corrupt zip archive: {e}")))?;
        let entry_name = entry.name().to_string();
        reject_zip_symlink(&entry, &entry_name)?;
        let rel = safe_zip_entry_path(&entry_name)?;

        if entry.is_dir() {
            if !rel.starts_with("files") {
                return Err(AppError::BadRequest("不是知识库导出包".into()));
            }
            std::fs::create_dir_all(destination.join(&rel))
                .map_err(|e| AppError::Internal(format!("failed to extract dir: {e}")))?;
            continue;
        }

        let allowed = rel == Path::new("manifest.json")
            || rel == Path::new("meta.json")
            || (rel.starts_with("files") && is_md(&rel));
        if !allowed {
            return Err(AppError::BadRequest(format!(
                "不是知识库导出包（包含不支持的条目: {entry_name}）"
            )));
        }

        let output_path = destination.join(&rel);
        // Defense in depth on top of component sanitization: the resolved
        // path must stay inside the extraction dir.
        if !output_path.starts_with(destination) {
            return Err(AppError::BadRequest(format!("非法压缩包条目: {entry_name}")));
        }
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::Internal(format!("failed to extract dirs: {e}")))?;
        }
        let mut output = std::fs::File::create(&output_path)
            .map_err(|e| AppError::Internal(format!("failed to extract file: {e}")))?;
        std::io::copy(&mut entry, &mut output)
            .map_err(|e| AppError::Internal(format!("failed to extract file: {e}")))?;
    }

    let manifest_bytes = std::fs::read(destination.join("manifest.json"))
        .map_err(|_| AppError::BadRequest("不是知识库导出包".into()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| AppError::BadRequest("不是知识库导出包".into()))?;
    validate_manifest(&manifest)?;

    let meta: ExportMeta = std::fs::read(destination.join("meta.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    Ok(meta)
}

/// Envelope check. Parsed as loose JSON so future manifests with extra
/// fields still pass — only `format`/`kind`/`version` are load-bearing.
fn validate_manifest(manifest: &serde_json::Value) -> Result<(), AppError> {
    let format = manifest.get("format").and_then(|v| v.as_str());
    let kind = manifest.get("kind").and_then(|v| v.as_str());
    if format != Some(EXPORT_FORMAT) || kind != Some(EXPORT_KIND) {
        return Err(AppError::BadRequest("不是知识库导出包".into()));
    }
    let version = manifest.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version > u64::from(EXPORT_VERSION) {
        return Err(AppError::BadRequest("导入包版本过新，请升级应用".into()));
    }
    Ok(())
}

/// Sanitize a zip entry name into a safe relative path (same policy as
/// `nomifun-extension`'s skill import): no backslashes, no absolute paths,
/// no `..`/prefix components.
fn safe_zip_entry_path(name: &str) -> Result<PathBuf, AppError> {
    let invalid = || AppError::BadRequest(format!("非法压缩包条目: {name}"));
    if name.is_empty() || name.contains('\\') {
        return Err(invalid());
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(invalid());
    }
    let mut safe_path = PathBuf::new();
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if !saw_normal {
                    let first = part.to_string_lossy();
                    let bytes = first.as_bytes();
                    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                        return Err(invalid());
                    }
                    saw_normal = true;
                }
                safe_path.push(part);
            }
            Component::CurDir => {}
            _ => return Err(invalid()),
        }
    }
    if safe_path.as_os_str().is_empty() {
        return Err(invalid());
    }
    Ok(safe_path)
}

fn reject_zip_symlink(entry: &zip::read::ZipFile<'_>, name: &str) -> Result<(), AppError> {
    if let Some(mode) = entry.unix_mode()
        && mode & 0o170000 == 0o120000
    {
        return Err(AppError::BadRequest(format!("非法压缩包条目: {name}")));
    }
    Ok(())
}

/// Move every file under `src_root` to the same relative path under
/// `dest_root` (rename with copy fallback). Missing `src_root` (a package
/// with zero files) is fine. Returns the number of files moved.
fn move_file_tree(src_root: &Path, dest_root: &Path) -> Result<u64, AppError> {
    if !src_root.is_dir() {
        return Ok(0);
    }
    let mut count = 0u64;
    for entry in walkdir::WalkDir::new(src_root) {
        let entry = entry.map_err(|e| AppError::Internal(format!("failed to walk imported files: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(src_root)
            .map_err(|e| AppError::Internal(format!("failed to relativize imported file: {e}")))?;
        let dest = dest_root.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::Internal(format!("failed to create import dirs: {e}")))?;
        }
        if std::fs::rename(entry.path(), &dest).is_err() {
            std::fs::copy(entry.path(), &dest)
                .map_err(|e| AppError::Internal(format!("failed to place imported file: {e}")))?;
        }
        count += 1;
    }
    Ok(count)
}

/// Suffix `name` with `" (2)"`, `" (3)"`, … until it no longer collides
/// with an existing base name.
fn dedup_name(existing: &HashSet<String>, name: &str) -> String {
    if !existing.contains(name) {
        return name.to_owned();
    }
    for n in 2u32.. {
        let candidate = format!("{name} ({n})");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 suffix space exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::make_service;

    fn write_test_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn manifest_json(version: u32, kind: &str) -> String {
        format!(
            r#"{{"format":"nomifun-export","version":{version},"kind":"{kind}","exported_at":0,"app_version":"0.0.0"}}"#
        )
    }

    #[tokio::test]
    async fn export_import_roundtrip_preserves_file_tree() {
        let dir = tempfile::TempDir::new().unwrap();
        let source = make_service(&dir.path().join("data-a"));
        let kb = source.create_base("迁移源库", "换机测试", None, None).await.unwrap();
        source.write_file(&kb.knowledge_base_id, "guide.md", "# 指南\n正文").await.unwrap();
        source.write_file(&kb.knowledge_base_id, "sub/notes.md", "嵌套内容").await.unwrap();
        source
            .write_file(
                &kb.knowledge_base_id,
                "_inbox/0190f5fe-7c00-7a00-8000-000000000001/draft.md",
                "# 草稿",
            )
            .await
            .unwrap();

        let zip_path = dir.path().join("out").join("kb.zip");
        let summary = export_base(&source, &kb.knowledge_base_id, &zip_path).await.unwrap();
        assert_eq!(summary.file_count, 3);
        assert!(summary.total_bytes > 0);
        assert!(zip_path.is_file());
        assert!(!dir.path().join("out").join("kb.zip.tmp").exists(), "tmp must be renamed away");

        // Import into a fresh service (the "new machine").
        let target = make_service(&dir.path().join("data-b"));
        let imported = import_base(&target, &zip_path).await.unwrap();
        assert_eq!(imported.name, "迁移源库");
        assert_eq!(imported.file_count, 3);

        let original: Vec<String> = source
            .list_files(&kb.knowledge_base_id)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.rel_path)
            .collect();
        let restored: Vec<String> = target
            .list_files(&imported.kb_id)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.rel_path)
            .collect();
        assert_eq!(original, restored);

        let content = target.read_file(&imported.kb_id, "guide.md").await.unwrap();
        assert_eq!(content.content, "# 指南\n正文");
        let info = target.get_base_info(&imported.kb_id).await.unwrap();
        assert_eq!(info.description, "换机测试");
        assert!(info.managed);
    }

    #[tokio::test]
    async fn import_rejects_zip_slip_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let zip_path = dir.path().join("evil.zip");
        write_test_zip(
            &zip_path,
            &[
                ("manifest.json", &manifest_json(1, EXPORT_KIND)),
                ("meta.json", r#"{"name":"x","description":""}"#),
                ("../evil.md", "escaped"),
            ],
        );

        let err = import_base(&service, &zip_path).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert!(!dir.path().join("evil.md").exists());
        assert!(
            service.list_bases().await.unwrap().is_empty(),
            "no base may be created from a rejected package"
        );
    }

    #[tokio::test]
    async fn import_rejects_wrong_kind_and_newer_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        let wrong_kind = dir.path().join("skills.zip");
        write_test_zip(
            &wrong_kind,
            &[
                ("manifest.json", &manifest_json(1, "skill-pack")),
                ("meta.json", r#"{"name":"x"}"#),
            ],
        );
        let err = import_base(&service, &wrong_kind).await.unwrap_err();
        assert!(err.to_string().contains("不是知识库导出包"), "{err}");

        let too_new = dir.path().join("future.zip");
        write_test_zip(
            &too_new,
            &[
                ("manifest.json", &manifest_json(2, EXPORT_KIND)),
                ("meta.json", r#"{"name":"x"}"#),
            ],
        );
        let err = import_base(&service, &too_new).await.unwrap_err();
        assert!(err.to_string().contains("导入包版本过新"), "{err}");

        let not_zip = dir.path().join("garbage.zip");
        std::fs::write(&not_zip, "definitely not a zip").unwrap();
        let err = import_base(&service, &not_zip).await.unwrap_err();
        assert!(err.to_string().contains("不是知识库导出包"), "{err}");
    }

    #[tokio::test]
    async fn import_rejects_non_md_payload() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let zip_path = dir.path().join("exe.zip");
        write_test_zip(
            &zip_path,
            &[
                ("manifest.json", &manifest_json(1, EXPORT_KIND)),
                ("meta.json", r#"{"name":"x"}"#),
                ("files/payload.exe", "MZ"),
            ],
        );
        let err = import_base(&service, &zip_path).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert!(service.list_bases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn import_suffixes_duplicate_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        service.create_base("我的库", "", None, None).await.unwrap();

        let zip_path = dir.path().join("dup.zip");
        write_test_zip(
            &zip_path,
            &[
                ("manifest.json", &manifest_json(1, EXPORT_KIND)),
                ("meta.json", r#"{"name":"我的库","description":""}"#),
                ("files/a.md", "# A"),
            ],
        );

        let first = import_base(&service, &zip_path).await.unwrap();
        assert_eq!(first.name, "我的库 (2)");
        let second = import_base(&service, &zip_path).await.unwrap();
        assert_eq!(second.name, "我的库 (3)");
    }

    #[test]
    fn dedup_name_picks_first_free_suffix() {
        let mut existing = HashSet::new();
        assert_eq!(dedup_name(&existing, "库"), "库");
        existing.insert("库".to_owned());
        assert_eq!(dedup_name(&existing, "库"), "库 (2)");
        existing.insert("库 (2)".to_owned());
        existing.insert("库 (3)".to_owned());
        assert_eq!(dedup_name(&existing, "库"), "库 (4)");
    }

    #[test]
    fn safe_zip_entry_path_policy() {
        assert!(safe_zip_entry_path("files/a.md").is_ok());
        assert!(safe_zip_entry_path("./files/a.md").is_ok());
        assert!(safe_zip_entry_path("../evil.md").is_err());
        assert!(safe_zip_entry_path("files/../../evil.md").is_err());
        assert!(safe_zip_entry_path("/abs.md").is_err());
        assert!(safe_zip_entry_path("files\\win.md").is_err());
        assert!(safe_zip_entry_path("").is_err());
        assert!(safe_zip_entry_path("C:/evil.md").is_err());
    }
}
