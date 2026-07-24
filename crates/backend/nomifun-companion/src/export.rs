//! Companion memory-bundle / companion-bundle zip export & import (spec §4.8) —
//! explicit cross-machine transfer for the shared memory hub and single companions.
//!
//! Package layouts (zip root), enveloped by a strict `manifest.json`:
//! - memory bundle (`kind: "memory"`): `memories.jsonl` (every companion_memories
//!   row, archived included), `learn_runs.jsonl`, `state.json`
//!   (`{"mood": …}`), optional raw `events/*.jsonl` day files.
//! - companion bundle (`kind: "companion"`): `companion.json` (full profile), `state.json`
//!   (`{"xp": …}`), `knowledge_refs.json` (`{"names": […]}` — binding names
//!   are collected by the frontend; this crate never touches the knowledge
//!   domain, and binding reconstruction after import is the frontend's job).
//!
//! Import mirrors the knowledge importer's hardening: component-sanitized
//! entry paths (zip-slip), symlink rejection, a strict entry whitelist, and a
//! manifest format/kind/version gate before anything is written. v3 packages
//! are accepted only at exactly version 3; payload JSON uses closed schemas.
//! Memory import is staged and committed in one SQLite transaction. Event files
//! use no-clobber publication and an existing same-name file is idempotent only
//! when both its SHA-256 and bytes are identical.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use nomifun_common::{AppError, TimestampMs, now_ms};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::profile::CompanionProfileConfig;
use crate::registry::CompanionRegistry;
use crate::service::CompanionService;
use crate::store::{CompanionLearnRun, CompanionMemory, CompanionStore};

/// v3 is a hard export/import baseline. Any other package version is rejected.
pub const EXPORT_FORMAT: &str = "nomifun-export";
pub const EXPORT_KIND_MEMORY: &str = "memory";
pub const EXPORT_KIND_COMPANION: &str = "companion";
pub const EXPORT_VERSION: u32 = 3;

/// Result of a successful export, returned to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    /// `"memory"` or `"companion"`.
    pub kind: String,
    /// Data entries written to the package (manifest excluded).
    pub file_count: u64,
    /// Uncompressed size of the packaged payload.
    pub total_bytes: u64,
    pub dest_path: String,
    /// Memory rows in the package (0 for companion bundles).
    pub memories: u64,
    /// Learn-run rows in the package (0 for companion bundles).
    pub learn_runs: u64,
    /// Raw `events/*.jsonl` files in the package (0 unless requested).
    pub event_files: u64,
}

/// Result of a successful import, returned to the frontend
/// (`{"kind":"memory",…}` / `{"kind":"companion",…}`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ImportOutcome {
    Memory {
        /// Memory rows inserted.
        imported: u64,
        /// Memory rows skipped as duplicates of local data.
        skipped_duplicates: u64,
    },
    Companion {
        companion_id: String,
        /// Final name after duplicate-name suffixing (`"name (2)"`, …).
        name: String,
        /// Echoed back verbatim from `knowledge_refs.json` so the frontend
        /// can rebuild knowledge bindings.
        knowledge_names: Vec<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportManifest {
    format: String,
    version: u32,
    kind: String,
    exported_at: TimestampMs,
    app_version: String,
}

fn manifest_for(kind: &str) -> ExportManifest {
    ExportManifest {
        format: EXPORT_FORMAT.to_owned(),
        version: EXPORT_VERSION,
        kind: kind.to_owned(),
        exported_at: now_ms(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// A required JSON field whose value may itself be null. A plain
/// `Option<String>` accepts a missing field, which is not valid for a v3
/// payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
struct RequiredOptionalString(Option<String>);

/// `state.json` of a memory bundle. Mood is parsed strictly but deliberately
/// not applied on import (the local machine's mood wins).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryStatePayload {
    mood: RequiredOptionalString,
}

/// `state.json` of a companion bundle.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompanionStatePayload {
    xp: i64,
}

/// `knowledge_refs.json` of a companion bundle.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeRefsPayload {
    names: Vec<String>,
}

// ── Roster access ───────────────────────────────────────────────────

/// The companion-roster operations a companion-bundle import needs. `CompanionService` is the
/// production implementation (live in-memory roster + WS events + default-companion
/// pointer); `CompanionRegistry` backs the tests. The registry itself must never be
/// re-scanned behind the service's back — going through the service keeps its
/// live map coherent.
#[async_trait::async_trait]
pub trait CompanionRoster: Send + Sync {
    async fn list_companions(&self) -> Vec<CompanionProfileConfig>;
    async fn create_companion(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError>;
    async fn patch_companion(
        &self,
        companion_id: &str,
        patch: serde_json::Value,
    ) -> Result<CompanionProfileConfig, AppError>;
    async fn remove_companion(&self, companion_id: &str) -> Result<(), AppError>;
}

#[async_trait::async_trait]
impl CompanionRoster for CompanionService {
    async fn list_companions(&self) -> Vec<CompanionProfileConfig> {
        CompanionService::list_companions(self).await
    }
    async fn create_companion(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError> {
        CompanionService::create_companion(self, name, character).await
    }
    async fn patch_companion(
        &self,
        companion_id: &str,
        patch: serde_json::Value,
    ) -> Result<CompanionProfileConfig, AppError> {
        CompanionService::patch_companion(self, companion_id, patch).await
    }
    async fn remove_companion(&self, companion_id: &str) -> Result<(), AppError> {
        CompanionService::delete_companion(self, companion_id).await
    }
}

#[async_trait::async_trait]
impl CompanionRoster for CompanionRegistry {
    async fn list_companions(&self) -> Vec<CompanionProfileConfig> {
        self.list().await
    }
    async fn create_companion(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError> {
        self.create(name, character).await
    }
    async fn patch_companion(
        &self,
        companion_id: &str,
        patch: serde_json::Value,
    ) -> Result<CompanionProfileConfig, AppError> {
        self.patch(companion_id, patch).await
    }
    async fn remove_companion(&self, companion_id: &str) -> Result<(), AppError> {
        self.remove(companion_id).await.map(|_| ())
    }
}

// ── Export ──────────────────────────────────────────────────────────

/// Package the shared memory hub (memories + learn runs + mood, optionally
/// the raw event day files) into a zip at `dest_path`, written atomically via
/// a same-directory tempfile + persist.
pub async fn export_memory_bundle(
    store: &CompanionStore,
    shared_dir: &Path,
    dest_path: &Path,
    include_events: bool,
) -> Result<ExportSummary, AppError> {
    if !dest_path.is_absolute() {
        return Err(AppError::BadRequest("dest_path must be absolute".into()));
    }
    let memories = store.dump_memories_all().await?;
    let learn_runs = store.dump_learn_runs_all().await?;
    let mood = store.get_state("mood").await?;

    let dest = dest_path.to_path_buf();
    let events_dir = include_events.then(|| shared_dir.join("events"));
    let memories_count = memories.len() as u64;
    let learn_runs_count = learn_runs.len() as u64;
    let (file_count, total_bytes, event_files) = tokio::task::spawn_blocking(move || {
        atomic_zip(&dest, |zip| {
            let mut total_bytes = 0u64;
            add_json_entry(zip, "manifest.json", &manifest_for(EXPORT_KIND_MEMORY))?;
            total_bytes += add_jsonl_entry(zip, "memories.jsonl", &memories)?;
            total_bytes += add_jsonl_entry(zip, "learn_runs.jsonl", &learn_runs)?;
            total_bytes += add_json_entry(
                zip,
                "state.json",
                &MemoryStatePayload {
                    mood: RequiredOptionalString(mood),
                },
            )?;
            let mut event_files = 0u64;
            if let Some(events_dir) = events_dir {
                let mut files = match std::fs::read_dir(&events_dir) {
                    Ok(entries) => entries
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| {
                            AppError::Internal(format!(
                                "failed to scan event directory {}: {error}",
                                events_dir.display()
                            ))
                        })?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => {
                        return Err(AppError::Internal(format!(
                            "failed to read event directory {}: {error}",
                            events_dir.display()
                        )));
                    }
                };
                let mut files = files
                    .drain(..)
                    .map(|entry| {
                        let file_type = entry.file_type().map_err(|error| {
                            AppError::Internal(format!(
                                "failed to inspect event entry {}: {error}",
                                entry.path().display()
                            ))
                        })?;
                        if !file_type.is_file() {
                            return Err(AppError::Internal(format!(
                                "event directory contains non-regular entry {}",
                                entry.path().display()
                            )));
                        }
                        let name = entry.file_name().into_string().map_err(|_| {
                            AppError::Internal(format!(
                                "event directory contains a non-UTF8 file name: {}",
                                entry.path().display()
                            ))
                        })?;
                        if !name.ends_with(".jsonl") {
                            return Err(AppError::Internal(format!(
                                "event directory contains unsupported file {name:?}"
                            )));
                        }
                        Ok(entry.path())
                    })
                    .collect::<Result<Vec<_>, AppError>>()?;
                files.sort();
                for path in files {
                    crate::collector::validate_event_file(&path)?;
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| AppError::Internal("event file name became non-UTF8".into()))?;
                    let bytes = std::fs::read(&path)
                        .map_err(|e| AppError::Internal(format!("failed to read event file {name}: {e}")))?;
                    add_raw_entry(zip, &format!("events/{name}"), &bytes)?;
                    total_bytes += bytes.len() as u64;
                    event_files += 1;
                }
            }
            Ok((3 + event_files, total_bytes, event_files))
        })
    })
    .await
    .map_err(|e| AppError::Internal(format!("export task join error: {e}")))??;

    Ok(ExportSummary {
        kind: EXPORT_KIND_MEMORY.to_owned(),
        file_count,
        total_bytes,
        dest_path: dest_path.to_string_lossy().to_string(),
        memories: memories_count,
        learn_runs: learn_runs_count,
        event_files,
    })
}

/// Package one companion (full profile + per-companion xp + knowledge binding names) into
/// a zip at `dest_path`. `knowledge_names` is supplied by the caller — the
/// binding list crosses domains and is collected on the frontend.
pub async fn export_companion_bundle(
    store: &CompanionStore,
    profile: &CompanionProfileConfig,
    dest_path: &Path,
    knowledge_names: &[String],
) -> Result<ExportSummary, AppError> {
    if !dest_path.is_absolute() {
        return Err(AppError::BadRequest("dest_path must be absolute".into()));
    }
    let xp = store.get_companion_state_i64(&profile.companion_id, "xp").await?;

    let dest = dest_path.to_path_buf();
    let profile = profile.clone();
    let refs = KnowledgeRefsPayload {
        names: knowledge_names.to_vec(),
    };
    let (file_count, total_bytes) = tokio::task::spawn_blocking(move || {
        atomic_zip(&dest, |zip| {
            let mut total_bytes = 0u64;
            add_json_entry(zip, "manifest.json", &manifest_for(EXPORT_KIND_COMPANION))?;
            total_bytes += add_json_entry(zip, "companion.json", &profile)?;
            total_bytes += add_json_entry(zip, "state.json", &CompanionStatePayload { xp })?;
            total_bytes += add_json_entry(zip, "knowledge_refs.json", &refs)?;
            Ok((3u64, total_bytes))
        })
    })
    .await
    .map_err(|e| AppError::Internal(format!("export task join error: {e}")))??;

    Ok(ExportSummary {
        kind: EXPORT_KIND_COMPANION.to_owned(),
        file_count,
        total_bytes,
        dest_path: dest_path.to_string_lossy().to_string(),
        memories: 0,
        learn_runs: 0,
        event_files: 0,
    })
}

/// Atomic zip write: parent dirs created, payload written to a securely-created
/// same-directory tempfile, fsynced, then persisted into place. A failed export
/// never leaves a half-written package behind.
fn atomic_zip<T>(
    dest: &Path,
    write: impl FnOnce(&mut zip::ZipWriter<std::fs::File>) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| AppError::Internal(format!("failed to create export dir: {e}")))?;
    let temp = tempfile::Builder::new()
        .prefix(".nomifun-export.")
        .tempfile_in(parent)
        .map_err(|e| AppError::Internal(format!("failed to create export tempfile: {e}")))?;
    let file = temp
        .reopen()
        .map_err(|e| AppError::Internal(format!("failed to reopen export tempfile: {e}")))?;
    let mut zip = zip::ZipWriter::new(file);
    let out = write(&mut zip)?;
    let file = zip
        .finish()
        .map_err(|e| AppError::Internal(format!("failed to write zip: {e}")))?;
    file.sync_all()
        .map_err(|e| AppError::Internal(format!("failed to fsync export file: {e}")))?;
    drop(file);
    temp.persist(dest)
        .map_err(|error| AppError::Internal(format!("failed to finalize export file: {}", error.error)))?;
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| AppError::Internal(format!("failed to fsync export directory: {e}")))?;
    }
    Ok(out)
}

/// Pretty-printed JSON entry; returns the payload size in bytes.
fn add_json_entry(
    zip: &mut zip::ZipWriter<std::fs::File>,
    name: &str,
    value: &impl Serialize,
) -> Result<u64, AppError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| AppError::Internal(e.to_string()))?;
    add_raw_entry(zip, name, &bytes)?;
    Ok(bytes.len() as u64)
}

/// One JSON object per line; returns the payload size in bytes.
fn add_jsonl_entry(
    zip: &mut zip::ZipWriter<std::fs::File>,
    name: &str,
    rows: &[impl Serialize],
) -> Result<u64, AppError> {
    let mut buf = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut buf, row).map_err(|e| AppError::Internal(e.to_string()))?;
        buf.push(b'\n');
    }
    add_raw_entry(zip, name, &buf)?;
    Ok(buf.len() as u64)
}

fn add_raw_entry(zip: &mut zip::ZipWriter<std::fs::File>, name: &str, bytes: &[u8]) -> Result<(), AppError> {
    zip.start_file(name, zip::write::SimpleFileOptions::default())
        .map_err(|e| AppError::Internal(format!("failed to write zip: {e}")))?;
    zip.write_all(bytes)
        .map_err(|e| AppError::Internal(format!("failed to package {name}: {e}")))?;
    Ok(())
}

// ── Import ──────────────────────────────────────────────────────────

/// Import a package created by [`export_memory_bundle`] or
/// [`export_companion_bundle`], dispatching on the manifest `kind`.
pub async fn import_bundle(
    store: &CompanionStore,
    roster: &dyn CompanionRoster,
    shared_dir: &Path,
    src_path: &Path,
) -> Result<ImportOutcome, AppError> {
    if !src_path.is_file() {
        return Err(AppError::BadRequest(format!(
            "import file does not exist: {}",
            src_path.display()
        )));
    }

    // Extraction temp lives under the shared dir (same volume as the events
    // destination), namespaced to avoid collisions.
    let tmp_root = shared_dir.join(".import-tmp");
    let extract_dir = tmp_root.join(format!("companion-{}-{}", std::process::id(), now_ms()));
    tokio::fs::create_dir_all(&extract_dir)
        .await
        .map_err(|e| AppError::Internal(format!("failed to create import temp dir: {e}")))?;

    let result = import_extracted(store, roster, shared_dir, src_path, &extract_dir).await;
    let _ = tokio::fs::remove_dir_all(&extract_dir).await;
    let _ = tokio::fs::remove_dir(&tmp_root).await; // best-effort, only when empty
    result
}

async fn import_extracted(
    store: &CompanionStore,
    roster: &dyn CompanionRoster,
    shared_dir: &Path,
    src_path: &Path,
    extract_dir: &Path,
) -> Result<ImportOutcome, AppError> {
    let src = src_path.to_path_buf();
    let dest = extract_dir.to_path_buf();
    let kind = tokio::task::spawn_blocking(move || extract_zip_validated(&src, &dest))
        .await
        .map_err(|e| AppError::Internal(format!("import task join error: {e}")))??;

    match kind.as_str() {
        EXPORT_KIND_MEMORY => import_memory_bundle(store, shared_dir, extract_dir).await,
        EXPORT_KIND_COMPANION => import_companion_bundle(store, roster, extract_dir).await,
        other => Err(AppError::BadRequest(format!("导入包类型不支持: {other}"))),
    }
}

/// Merge a memory bundle into the local store. Both jsonl files are parsed
/// fully before a SQLite transaction is opened. Event files are also
/// preflighted before the transaction: a same-name local file must have both
/// the same SHA-256 and the same bytes. New event files are published with
/// no-clobber hard links while the transaction remains uncommitted; any
/// publication failure rolls back both the DB rows and files created by this
/// attempt. The packaged mood is deliberately ignored.
async fn import_memory_bundle(
    store: &CompanionStore,
    shared_dir: &Path,
    extract_dir: &Path,
) -> Result<ImportOutcome, AppError> {
    let memories = parse_jsonl::<CompanionMemory>(&extract_dir.join("memories.jsonl"), "memories.jsonl", true)?;
    let learn_runs =
        parse_jsonl::<CompanionLearnRun>(&extract_dir.join("learn_runs.jsonl"), "learn_runs.jsonl", true)?;
    let _state: MemoryStatePayload = read_json_strict(&extract_dir.join("state.json"), "state.json")?;
    let event_plan = plan_event_import(&extract_dir.join("events"), &shared_dir.join("events"))?;

    let transaction = store.begin_memory_import(&memories, &learn_runs).await?;
    let stats = transaction.stats();
    let published = match publish_event_import(&event_plan) {
        Ok(published) => published,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    };
    match transaction.commit().await {
        Ok(_) => Ok(ImportOutcome::Memory {
            imported: stats.imported,
            skipped_duplicates: stats.skipped_duplicates,
        }),
        Err(error) => {
            published.rollback();
            Err(error)
        }
    }
}

/// Recreate a packaged companion through the live roster: `create` (validated name,
/// deduplicated against existing companions) + `patch` (persona/model/appearance),
/// then the per-companion xp. Any failure after creation rolls the new companion back.
async fn import_companion_bundle(
    store: &CompanionStore,
    roster: &dyn CompanionRoster,
    extract_dir: &Path,
) -> Result<ImportOutcome, AppError> {
    let companion_bytes = std::fs::read(extract_dir.join("companion.json"))
        .map_err(|_| AppError::BadRequest("导出包缺少 companion.json".into()))?;
    let profile: CompanionProfileConfig =
        serde_json::from_slice(&companion_bytes).map_err(|e| AppError::BadRequest(format!("companion.json 无法解析: {e}")))?;
    nomifun_common::CompanionId::try_from(profile.companion_id.as_str())
        .map_err(|error| AppError::BadRequest(format!("companion.json companion_id 无效: {error}")))?;
    if profile.seq == 0 {
        return Err(AppError::BadRequest("companion.json seq 必须大于 0".into()));
    }
    let state: CompanionStatePayload = read_json_strict(&extract_dir.join("state.json"), "state.json")?;
    let refs: KnowledgeRefsPayload =
        read_json_strict(&extract_dir.join("knowledge_refs.json"), "knowledge_refs.json")?;

    let existing: HashSet<String> = roster.list_companions().await.into_iter().map(|p| p.name).collect();
    let base_name = match profile.name.trim() {
        "" => "导入的伙伴",
        name => name,
    };
    let final_name = dedup_name(&existing, base_name);

    let created = roster.create_companion(&final_name, &profile.character).await?;
    let setup = async {
        roster
            .patch_companion(
                &created.companion_id,
                serde_json::json!({
                    "persona": profile.persona,
                    "model": profile.model,
                    "appearance": profile.appearance,
                }),
            )
            .await?;
        if state.xp != 0 {
            store.set_companion_state(&created.companion_id, "xp", &state.xp.to_string()).await?;
        }
        Ok::<(), AppError>(())
    }
    .await;
    if let Err(e) = setup {
        // Roll back the half-imported companion; a failed rollback only warns.
        if let Err(cleanup) = store.delete_companion_rows(&created.companion_id).await {
            tracing::warn!(
                companion_id = %created.companion_id,
                error = %cleanup,
                "rollback of failed companion import left stale store rows"
            );
        }
        if let Err(del) = roster.remove_companion(&created.companion_id).await {
            tracing::warn!(companion_id = %created.companion_id, error = %del, "rollback of failed companion import left a stale companion");
        }
        return Err(e);
    }

    Ok(ImportOutcome::Companion {
        companion_id: created.companion_id,
        name: final_name,
        knowledge_names: refs.names,
    })
}

/// Parse one jsonl file into rows, strictly: any malformed line fails the
/// whole import before anything was written. `required` distinguishes a
/// mandatory file (missing → BadRequest) from an optional one (missing →
/// empty).
fn parse_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path,
    label: &str,
    required: bool,
) -> Result<Vec<T>, AppError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(Vec::new());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::BadRequest(format!("导出包缺少 {label}")));
        }
        Err(error) => {
            return Err(AppError::Internal(format!("检查 {label} 失败: {error}")));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(AppError::BadRequest(format!("{label} 必须是普通文件")));
    }
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) => {
            return Err(AppError::Internal(format!("读取 {label} 失败: {error}")));
        }
    };
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if !raw.is_empty() && !raw.ends_with(b"\n") {
        return Err(AppError::BadRequest(format!("{label} 末行不完整")));
    }
    let mut rows = Vec::new();
    let lines: Vec<&[u8]> = raw.split(|byte| *byte == b'\n').collect();
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            if index + 1 == lines.len() && raw.ends_with(b"\n") {
                continue;
            }
            return Err(AppError::BadRequest(format!(
                "{label} 第 {} 行为空记录",
                index + 1
            )));
        }
        let row: T = serde_json::from_slice(line)
            .map_err(|e| AppError::BadRequest(format!("{label} 第 {} 行无法解析: {e}", index + 1)))?;
        rows.push(row);
    }
    Ok(rows)
}

fn read_json_strict<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> Result<T, AppError> {
    let bytes = std::fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::BadRequest(format!("导出包缺少 {label}"))
        } else {
            AppError::Internal(format!("读取 {label} 失败: {error}"))
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|error| AppError::BadRequest(format!("{label} 无法解析: {error}")))
}

#[derive(Debug)]
struct EventImportPlan {
    source: PathBuf,
    target: PathBuf,
}

#[derive(Debug)]
struct PublishedEvents {
    targets: Vec<PathBuf>,
}

impl PublishedEvents {
    fn rollback(self) {
        let mut parents = std::collections::HashSet::new();
        for target in self.targets.into_iter().rev() {
            if let Some(parent) = target.parent() {
                parents.insert(parent.to_path_buf());
            }
            if let Err(error) = crate::fsio::remove_path_entry(&target) {
                tracing::warn!(path = %target.display(), %error, "failed to roll back imported event file");
            }
        }
        for parent in parents {
            if let Err(error) = crate::fsio::sync_dir(&parent) {
                tracing::warn!(path = %parent.display(), %error, "failed to fsync rolled-back event directory");
            }
        }
    }
}

/// Build a deterministic publication plan, strictly validate every imported
/// event JSONL file, and reject every existing same-name event whose digest or
/// bytes differ. Comparing bytes after SHA-256 avoids treating a theoretical
/// hash collision as identical content.
fn plan_event_import(package_dir: &Path, destination_dir: &Path) -> Result<Vec<EventImportPlan>, AppError> {
    if !package_dir.exists() {
        return Ok(Vec::new());
    }
    if !package_dir.is_dir() {
        return Err(AppError::BadRequest("events 必须是目录".into()));
    }

    let mut sources = std::fs::read_dir(package_dir)
        .map_err(|error| AppError::Internal(format!("读取导入 events 目录失败: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::Internal(format!("读取导入 event 条目失败: {error}")))?;
    sources.sort_by_key(std::fs::DirEntry::file_name);

    let mut plan = Vec::new();
    for entry in sources {
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::Internal(format!("检查导入 event 类型失败: {error}")))?;
        if !file_type.is_file() {
            return Err(AppError::BadRequest(format!(
                "events 包含非普通文件: {}",
                entry.file_name().to_string_lossy()
            )));
        }
        let source = entry.path();
        if source.extension().is_none_or(|extension| extension != "jsonl") {
            return Err(AppError::BadRequest(format!(
                "events 包含非 jsonl 文件: {}",
                entry.file_name().to_string_lossy()
            )));
        }
        crate::collector::validate_event_file(&source).map_err(|error| {
            AppError::BadRequest(format!(
                "导入 event 文件 {} 损坏: {error}",
                entry.file_name().to_string_lossy()
            ))
        })?;
        let target = destination_dir.join(entry.file_name());
        match std::fs::read(&target) {
            Ok(local) => {
                crate::collector::validate_event_file(&target)?;
                let imported = std::fs::read(&source)
                    .map_err(|error| AppError::Internal(format!("读取导入 event 文件失败: {error}")))?;
                if sha256_bytes(&local) != sha256_bytes(&imported) || local != imported {
                    return Err(AppError::Conflict(format!(
                        "event import conflict for {}: local and imported hash/content differ",
                        entry.file_name().to_string_lossy()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.push(EventImportPlan { source, target });
            }
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "读取本地 event 文件 {} 失败: {error}",
                    target.display()
                )));
            }
        }
    }
    Ok(plan)
}

/// Publish staged event files without overwriting anything. A hard link is an
/// atomic no-clobber operation and keeps the extracted staging directory alive
/// until the import transaction is committed or rolled back.
fn publish_event_import(plan: &[EventImportPlan]) -> Result<PublishedEvents, AppError> {
    let Some(destination_dir) = plan.first().and_then(|entry| entry.target.parent()) else {
        return Ok(PublishedEvents { targets: Vec::new() });
    };
    std::fs::create_dir_all(destination_dir)
        .map_err(|error| AppError::Internal(format!("创建 events 目录失败: {error}")))?;

    let mut published = PublishedEvents { targets: Vec::new() };
    for entry in plan {
        match std::fs::hard_link(&entry.source, &entry.target) {
            Ok(()) => published.targets.push(entry.target.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let local = std::fs::read(&entry.target).map_err(|read_error| {
                    AppError::Internal(format!(
                        "读取并发创建的 event 文件 {} 失败: {read_error}",
                        entry.target.display()
                    ))
                })?;
                let imported = std::fs::read(&entry.source)
                    .map_err(|read_error| AppError::Internal(format!("读取导入 event 文件失败: {read_error}")))?;
                if sha256_bytes(&local) != sha256_bytes(&imported) || local != imported {
                    published.rollback();
                    return Err(AppError::Conflict(format!(
                        "event import conflict for {}: local and imported hash/content differ",
                        entry.target.file_name().unwrap_or_default().to_string_lossy()
                    )));
                }
            }
            Err(error) => {
                published.rollback();
                return Err(AppError::Internal(format!(
                    "发布 event 文件 {} 失败: {error}",
                    entry.target.display()
                )));
            }
        }
    }
    crate::fsio::sync_dir(destination_dir)
        .map_err(|error| AppError::Internal(format!("fsync imported events directory: {error}")))?;
    Ok(published)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Blocking extraction with validation. Only the documented package entries
/// are accepted (`manifest.json`, `memories.jsonl`, `learn_runs.jsonl`,
/// `state.json`, `companion.json`, `knowledge_refs.json`, `events/*.jsonl`); every
/// entry path is sanitized (zip-slip) and symlink entries are rejected.
/// Returns the manifest `kind` after the format/version checks passed.
fn extract_zip_validated(archive_path: &Path, destination: &Path) -> Result<String, AppError> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| AppError::BadRequest(format!("failed to open import file: {e}")))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|_| AppError::BadRequest("不是 NomiFun 导出包".into()))?;

    let mut seen_entries = HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| AppError::BadRequest(format!("corrupt zip archive: {e}")))?;
        let entry_name = entry.name().to_string();
        reject_zip_symlink(&entry, &entry_name)?;
        let rel = safe_zip_entry_path(&entry_name)?;
        if !seen_entries.insert(rel.clone()) {
            return Err(AppError::BadRequest(format!(
                "导出包包含重复条目: {entry_name}"
            )));
        }

        if entry.is_dir() {
            if rel != Path::new("events") {
                return Err(AppError::BadRequest(format!(
                    "不是 NomiFun 导出包（包含不支持的条目: {entry_name}）"
                )));
            }
            std::fs::create_dir_all(destination.join(&rel))
                .map_err(|e| AppError::Internal(format!("failed to extract dir: {e}")))?;
            continue;
        }

        let allowed = rel == Path::new("manifest.json")
            || rel == Path::new("memories.jsonl")
            || rel == Path::new("learn_runs.jsonl")
            || rel == Path::new("state.json")
            || rel == Path::new("companion.json")
            || rel == Path::new("knowledge_refs.json")
            || (rel.parent() == Some(Path::new("events")) && rel.extension().is_some_and(|ext| ext == "jsonl"));
        if !allowed {
            return Err(AppError::BadRequest(format!(
                "不是 NomiFun 导出包（包含不支持的条目: {entry_name}）"
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
        .map_err(|_| AppError::BadRequest("不是 NomiFun 导出包".into()))?;
    let manifest: ExportManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| AppError::BadRequest(format!("manifest.json 无法解析: {error}")))?;
    validate_manifest(&manifest, destination)
}

/// Envelope and package-shape check. The version must be exactly 3:
/// missing/zero/lower and future versions all fail closed.
fn validate_manifest(manifest: &ExportManifest, destination: &Path) -> Result<String, AppError> {
    if manifest.format != EXPORT_FORMAT {
        return Err(AppError::BadRequest("不是 NomiFun 导出包".into()));
    }
    if manifest.version != EXPORT_VERSION {
        if manifest.version > EXPORT_VERSION {
            return Err(AppError::BadRequest("导入包版本过新，请升级应用".into()));
        }
        return Err(AppError::BadRequest("导入包版本过旧，必须使用精确 v3".into()));
    }
    let kind = match manifest.kind.as_str() {
        EXPORT_KIND_MEMORY | EXPORT_KIND_COMPANION => manifest.kind.clone(),
        other => return Err(AppError::BadRequest(format!("导入包类型不支持: {other}"))),
    };
    let present = |name: &str| destination.join(name).is_file();
    let valid_shape = match kind.as_str() {
        EXPORT_KIND_MEMORY => {
            present("memories.jsonl")
                && present("learn_runs.jsonl")
                && present("state.json")
                && !present("companion.json")
                && !present("knowledge_refs.json")
        }
        EXPORT_KIND_COMPANION => {
            present("companion.json")
                && present("state.json")
                && present("knowledge_refs.json")
                && !present("memories.jsonl")
                && !present("learn_runs.jsonl")
                && !destination.join("events").exists()
        }
        _ => false,
    };
    if !valid_shape {
        return Err(AppError::BadRequest(format!(
            "v3 {kind} 导出包文件集合不完整或包含错误条目"
        )));
    }
    Ok(kind)
}

/// Sanitize a zip entry name into a safe relative path (same policy as the
/// knowledge/skill importers): no backslashes, no absolute paths, no
/// `..`/prefix components.
fn safe_zip_entry_path(name: &str) -> Result<PathBuf, AppError> {
    let invalid = || AppError::BadRequest(format!("非法压缩包条目: {name}"));
    // Backslashes and colons are rejected at the byte level: a Windows drive
    // prefix ("C:/…") parses as `Component::Prefix` only on Windows — on
    // Unix it is a plain `Normal` component, so a byte check is the only
    // portable way to hold the no-drive-prefix policy on every platform.
    // (Our own exporter never writes either byte into an entry name.)
    if name.is_empty() || name.contains('\\') || name.contains(':') {
        return Err(invalid());
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(invalid());
    }
    let mut safe_path = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe_path.push(part),
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

/// Suffix `name` with `" (2)"`, `" (3)"`, … until it no longer collides
/// with an existing companion name.
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
    use crate::registry::CompanionRegistry;

    fn memory_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::CompanionMemoryId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn provider_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ProviderId::try_from(raw.as_str()).unwrap().into_string()
    }

    /// Registry over `{root}/{companions}` with its seq-watermark state beside it
    /// at `{root}/{companions}-shared` (each test roster gets its own watermark).
    fn scan_registry(root: &Path, companions: &str) -> CompanionRegistry {
        CompanionRegistry::scan(
            root.join(companions),
            root.join(format!("{companions}-shared")),
        )
        .unwrap()
    }

    fn raw_memory(memory_id: &str, kind: &str, content: &str, status: &str) -> CompanionMemory {
        CompanionMemory {
            memory_id: memory_id.to_owned(),
            kind: kind.to_owned(),
            content: content.to_owned(),
            tags: vec!["标签".into()],
            importance: 0.8,
            strength: 0.42,
            pinned: kind == "preference",
            source: "manual".into(),
            status: status.to_owned(),
            created_at: 1_111,
            updated_at: 2_222,
            last_reinforced_at: 3_333,
            scope_kind: "user".into(),
            scope_companion_id: None,
        }
    }

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

    fn sorted_json(memories: &mut Vec<CompanionMemory>) -> serde_json::Value {
        memories.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
        serde_json::to_value(&*memories).unwrap()
    }

    #[tokio::test]
    async fn memory_bundle_roundtrip_full_fidelity_and_dedup() {
        let dir = tempfile::TempDir::new().unwrap();
        let shared_a = dir.path().join("shared-a");
        std::fs::create_dir_all(shared_a.join("events")).unwrap();
        let event_line = format!(
            r#"{{"event_id":"{}","ts":1,"source":"chat","name":"x","data":{{}}}}"#,
            nomifun_common::generate_id()
        );
        std::fs::write(shared_a.join("events").join("20260601.jsonl"), format!("{event_line}\n")).unwrap();

        let store_a = CompanionStore::open_memory().await.unwrap();
        let mut originals = vec![
            raw_memory(&memory_fixture(1), "preference", "主人喜欢深色主题", "active"),
            raw_memory(&memory_fixture(2), "episode", "上周修了导出 bug", "archived"),
            raw_memory(&memory_fixture(3), "knowledge", "cargo test -p nomifun-companion 是门禁", "active"),
        ];
        for m in &originals {
            store_a.insert_memory_raw(m).await.unwrap();
        }
        store_a.set_state("mood", "happy").await.unwrap();
        let learn_run_id = nomifun_common::CompanionLearnRunId::new().into_string();
        store_a
            .insert_learn_run(&CompanionLearnRun {
                learn_run_id: learn_run_id.clone(),
                started_at: 10,
                finished_at: Some(20),
                status: "ok".into(),
                events_processed: 5,
                memories_added: 2,
                suggestions_added: 1,
                error: None,
                summary: Some("学到了".into()),
            })
            .await
            .unwrap();

        let zip_path = dir.path().join("out").join("memory.zip");
        let summary = export_memory_bundle(&store_a, &shared_a, &zip_path, true).await.unwrap();
        assert_eq!(summary.kind, "memory");
        assert_eq!(summary.memories, 3);
        assert_eq!(summary.learn_runs, 1);
        assert_eq!(summary.event_files, 1);
        assert_eq!(summary.file_count, 4);
        assert!(summary.total_bytes > 0);
        assert!(zip_path.is_file());
        assert!(
            !dir.path().join("out").join("memory.zip.tmp").exists(),
            "tmp must be renamed away"
        );

        // Import into a fresh machine: full fidelity, mood untouched.
        let shared_b = dir.path().join("shared-b");
        let store_b = CompanionStore::open_memory().await.unwrap();
        store_b.set_state("mood", "calm").await.unwrap();
        let roster_b = scan_registry(dir.path(), "companions-b");
        let outcome = import_bundle(&store_b, &roster_b, &shared_b, &zip_path).await.unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Memory {
                imported: 3,
                skipped_duplicates: 0
            }
        );

        let mut restored = store_b.dump_memories_all().await.unwrap();
        assert_eq!(sorted_json(&mut restored), sorted_json(&mut originals));
        assert_eq!(store_b.get_state("mood").await.unwrap().as_deref(), Some("calm"));
        assert!(store_b.learn_run_exists(&learn_run_id).await.unwrap());
        let landed = shared_b.join("events").join("20260601.jsonl");
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), format!("{event_line}\n"));

        // Re-import with byte-identical events: everything (incl. the archived
        // row and event file) is idempotently skipped.
        let outcome = import_bundle(&store_b, &roster_b, &shared_b, &zip_path).await.unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Memory {
                imported: 0,
                skipped_duplicates: 3
            }
        );
        assert_eq!(store_b.dump_memories_all().await.unwrap().len(), 3);
        assert_eq!(store_b.dump_learn_runs_all().await.unwrap().len(), 1);

        // A same-name event is never silently preferred. Different hash or
        // bytes is a hard conflict and leaves both DB and local file unchanged.
        let local_event = format!(
            "{{\"event_id\":\"{}\",\"ts\":2,\"source\":\"chat\",\"name\":\"local\",\"data\":{{}}}}\n",
            nomifun_common::generate_id()
        );
        std::fs::write(&landed, &local_event).unwrap();
        let error = import_bundle(&store_b, &roster_b, &shared_b, &zip_path)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("event import conflict"), "{error}");
        assert_eq!(store_b.dump_memories_all().await.unwrap().len(), 3);
        assert_eq!(store_b.dump_learn_runs_all().await.unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), local_event);
    }

    #[tokio::test]
    async fn memory_import_rejects_same_id_with_different_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let shared_a = dir.path().join("shared-a");
        let store_a = CompanionStore::open_memory().await.unwrap();
        let clashing_id = memory_fixture(10);
        store_a
            .insert_memory_raw(&raw_memory(&clashing_id, "knowledge", "来自源机器的知识", "active"))
            .await
            .unwrap();
        let zip_path = dir.path().join("clash.zip");
        export_memory_bundle(&store_a, &shared_a, &zip_path, false).await.unwrap();

        // Target machine already owns mem_clash with different content. Merge
        // must fail rather than silently changing global identity.
        let store_b = CompanionStore::open_memory().await.unwrap();
        store_b
            .insert_memory_raw(&raw_memory(&clashing_id, "knowledge", "本机完全不同的知识", "active"))
            .await
            .unwrap();
        let roster_b = scan_registry(dir.path(), "companions-b");
        let error = import_bundle(&store_b, &roster_b, &dir.path().join("shared-b"), &zip_path)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("memory import ID collision"));
        let all = store_b.dump_memories_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].memory_id, clashing_id);
    }

    #[test]
    fn parse_jsonl_rejects_partial_empty_and_unknown_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memories.jsonl");
        let memory = raw_memory(&memory_fixture(20), "knowledge", "严格 JSONL", "active");
        let line = serde_json::to_string(&memory).unwrap();

        std::fs::write(&path, &line).unwrap();
        let error = parse_jsonl::<CompanionMemory>(&path, "memories.jsonl", true).unwrap_err();
        assert!(error.to_string().contains("末行不完整"), "{error}");

        std::fs::write(&path, format!("{line}\n\n")).unwrap();
        let error = parse_jsonl::<CompanionMemory>(&path, "memories.jsonl", true).unwrap_err();
        assert!(error.to_string().contains("为空记录"), "{error}");

        let mut unknown = serde_json::to_value(&memory).unwrap();
        unknown["retired_field"] = serde_json::json!(true);
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&unknown).unwrap()))
            .unwrap();
        let error = parse_jsonl::<CompanionMemory>(&path, "memories.jsonl", true).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[tokio::test]
    async fn companion_bundle_roundtrip_keeps_xp_suffixes_name_and_echoes_refs() {
        let dir = tempfile::TempDir::new().unwrap();
        let store_a = CompanionStore::open_memory().await.unwrap();
        let reg_a = scan_registry(dir.path(), "companions-a");
        let created = reg_a.create("毛球", "ink").await.unwrap();
        let provider_id = provider_fixture(1);
        let profile = reg_a
            .patch(
                &created.companion_id,
                serde_json::json!({
                    "persona": {"preset": "sassy", "custom": "喜欢用颜文字"},
                    "model": {"provider_id": provider_id, "model": "claude-fable-5"},
                    "appearance": {"companion_enabled": true, "companion_x": 7, "quiet_start": "22:00", "quiet_end": "08:00"}
                }),
            )
            .await
            .unwrap();
        store_a.add_companion_xp(&profile.companion_id, 57).await.unwrap();

        let zip_path = dir.path().join("companion.zip");
        let summary = export_companion_bundle(&store_a, &profile, &zip_path, &["库甲".into(), "库乙".into()])
            .await
            .unwrap();
        assert_eq!(summary.kind, "companion");
        assert_eq!(summary.file_count, 3);
        assert!(!dir.path().join("companion.zip.tmp").exists());

        // Target roster already has a companion with the same name.
        let store_b = CompanionStore::open_memory().await.unwrap();
        let reg_b = scan_registry(dir.path(), "companions-b");
        reg_b.create("毛球", "mochi").await.unwrap();

        let outcome = import_bundle(&store_b, &reg_b, &dir.path().join("shared-b"), &zip_path)
            .await
            .unwrap();
        let ImportOutcome::Companion {
            companion_id,
            name,
            knowledge_names,
        } = outcome
        else {
            panic!("expected companion outcome");
        };
        assert_eq!(name, "毛球 (2)");
        assert_eq!(knowledge_names, vec!["库甲".to_string(), "库乙".to_string()]);
        assert_ne!(
            companion_id, profile.companion_id,
            "imported companion gets a fresh companion_id"
        );

        let imported = reg_b.get(&companion_id).await.unwrap();
        assert_eq!(imported.name, "毛球 (2)");
        // A fresh local short number is allocated (the bundle's own seq is
        // ignored): "毛球" took 1 on this roster, so the import gets 2.
        assert_eq!(imported.seq, 2);
        assert_eq!(imported.character, "ink");
        assert_eq!(imported.persona, profile.persona);
        assert_eq!(imported.model, profile.model);
        assert_eq!(imported.appearance, profile.appearance);
        assert_eq!(store_b.get_companion_state_i64(&companion_id, "xp").await.unwrap(), 57);

        // Importing again suffixes further.
        let outcome = import_bundle(&store_b, &reg_b, &dir.path().join("shared-b"), &zip_path)
            .await
            .unwrap();
        let ImportOutcome::Companion { name, .. } = outcome else {
            panic!("expected companion outcome");
        };
        assert_eq!(name, "毛球 (3)");
    }

    #[tokio::test]
    async fn import_accepts_only_exact_v3_and_rejects_invalid_envelopes() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let roster = scan_registry(dir.path(), "companions");
        let shared = dir.path().join("shared");

        let wrong_format = dir.path().join("format.zip");
        write_test_zip(
            &wrong_format,
            &[
                (
                    "manifest.json",
                    r#"{"format":"other-export","version":3,"kind":"memory","exported_at":0,"app_version":"0.0.0"}"#,
                ),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &wrong_format).await.unwrap_err();
        assert!(err.to_string().contains("不是 NomiFun 导出包"), "{err}");

        let wrong_kind = dir.path().join("kind.zip");
        write_test_zip(&wrong_kind, &[("manifest.json", &manifest_json(3, "knowledge-base"))]);
        let err = import_bundle(&store, &roster, &shared, &wrong_kind).await.unwrap_err();
        assert!(err.to_string().contains("导入包类型不支持"), "{err}");

        let too_new = dir.path().join("future.zip");
        write_test_zip(&too_new, &[("manifest.json", &manifest_json(4, EXPORT_KIND_MEMORY))]);
        let err = import_bundle(&store, &roster, &shared, &too_new).await.unwrap_err();
        assert!(err.to_string().contains("导入包版本过新"), "{err}");

        for (name, manifest) in [
            (
                "missing-version",
                r#"{"format":"nomifun-export","kind":"memory","exported_at":0,"app_version":"0.0.0"}"#,
            ),
            (
                "zero-version",
                r#"{"format":"nomifun-export","version":0,"kind":"memory","exported_at":0,"app_version":"0.0.0"}"#,
            ),
            (
                "low-version",
                r#"{"format":"nomifun-export","version":2,"kind":"memory","exported_at":0,"app_version":"0.0.0"}"#,
            ),
        ] {
            let path = dir.path().join(format!("{name}.zip"));
            write_test_zip(
                &path,
                &[
                    ("manifest.json", manifest),
                    ("memories.jsonl", ""),
                    ("learn_runs.jsonl", ""),
                    ("state.json", r#"{"mood":null}"#),
                ],
            );
            let err = import_bundle(&store, &roster, &shared, &path).await.unwrap_err();
            assert!(
                err.to_string().contains("manifest.json") || err.to_string().contains("版本过旧"),
                "{name}: {err}"
            );
        }

        let unknown_manifest_field = dir.path().join("manifest-extra.zip");
        write_test_zip(
            &unknown_manifest_field,
            &[
                (
                    "manifest.json",
                    r#"{"format":"nomifun-export","version":3,"kind":"memory","exported_at":0,"app_version":"0.0.0","extra":true}"#,
                ),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &unknown_manifest_field)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");

        let not_zip = dir.path().join("garbage.zip");
        std::fs::write(&not_zip, "definitely not a zip").unwrap();
        let err = import_bundle(&store, &roster, &shared, &not_zip).await.unwrap_err();
        assert!(err.to_string().contains("不是 NomiFun 导出包"), "{err}");

        let missing = dir.path().join("missing.zip");
        let err = import_bundle(&store, &roster, &shared, &missing).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");

        // A memory package without memories.jsonl is rejected explicitly.
        let incomplete = dir.path().join("incomplete.zip");
        write_test_zip(
            &incomplete,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &incomplete).await.unwrap_err();
        assert!(err.to_string().contains("文件集合"), "{err}");
        assert_eq!(store.dump_memories_all().await.unwrap().len(), 0);
        assert!(roster.list().await.is_empty());
    }

    #[tokio::test]
    async fn import_requires_strict_state_and_knowledge_ref_schemas() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let roster = scan_registry(dir.path(), "companions");
        let shared = dir.path().join("shared");
        let profile = CompanionProfileConfig::new("严格伙伴", "ink", 1);
        let profile_json = serde_json::to_string(&profile).unwrap();

        let memory_state_missing = dir.path().join("memory-state-missing.zip");
        write_test_zip(
            &memory_state_missing,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
            ],
        );
        let error = import_bundle(&store, &roster, &shared, &memory_state_missing)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("文件集合"), "{error}");
        assert!(roster.list().await.is_empty());

        let memory_state_unknown = dir.path().join("memory-state-unknown.zip");
        write_test_zip(
            &memory_state_unknown,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null,"extra":true}"#),
            ],
        );
        let error = import_bundle(&store, &roster, &shared, &memory_state_unknown)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");

        let companion_state_missing = dir.path().join("companion-state-missing.zip");
        write_test_zip(
            &companion_state_missing,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_COMPANION)),
                ("companion.json", &profile_json),
                ("knowledge_refs.json", r#"{"names":[]}"#),
            ],
        );
        let error = import_bundle(&store, &roster, &shared, &companion_state_missing)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("文件集合"), "{error}");
        assert!(roster.list().await.is_empty());

        let companion_state_unknown = dir.path().join("companion-state-unknown.zip");
        write_test_zip(
            &companion_state_unknown,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_COMPANION)),
                ("companion.json", &profile_json),
                ("state.json", r#"{"xp":0,"extra":true}"#),
                ("knowledge_refs.json", r#"{"names":[]}"#),
            ],
        );
        let error = import_bundle(&store, &roster, &shared, &companion_state_unknown)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
        assert!(roster.list().await.is_empty());

        let refs_unknown = dir.path().join("knowledge-refs-unknown.zip");
        write_test_zip(
            &refs_unknown,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_COMPANION)),
                ("companion.json", &profile_json),
                ("state.json", r#"{"xp":0}"#),
                ("knowledge_refs.json", r#"{"names":[],"extra":true}"#),
            ],
        );
        let error = import_bundle(&store, &roster, &shared, &refs_unknown)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
        assert!(roster.list().await.is_empty());
    }

    #[tokio::test]
    async fn memory_import_rolls_back_staged_rows_when_a_later_conflict_is_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let roster = scan_registry(dir.path(), "companions");
        let conflict_id = nomifun_common::CompanionLearnRunId::new().into_string();
        store
            .insert_learn_run(&CompanionLearnRun {
                learn_run_id: conflict_id.clone(),
                started_at: 1,
                finished_at: Some(2),
                status: "local".into(),
                events_processed: 0,
                memories_added: 0,
                suggestions_added: 0,
                error: None,
                summary: Some("local".into()),
            })
            .await
            .unwrap();

        let imported_memory = raw_memory(&memory_fixture(50), "knowledge", "先写入事务再触发冲突", "active");
        let imported_run = CompanionLearnRun {
            learn_run_id: conflict_id,
            started_at: 9,
            finished_at: Some(10),
            status: "imported".into(),
            events_processed: 1,
            memories_added: 1,
            suggestions_added: 0,
            error: None,
            summary: Some("不同内容".into()),
        };
        let archive = dir.path().join("rollback.zip");
        write_test_zip(
            &archive,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", &format!("{}\n", serde_json::to_string(&imported_memory).unwrap())),
                ("learn_runs.jsonl", &format!("{}\n", serde_json::to_string(&imported_run).unwrap())),
                ("state.json", r#"{"mood":null}"#),
            ],
        );

        let error = import_bundle(&store, &roster, &dir.path().join("shared"), &archive)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("learn-run import ID collision"), "{error}");
        assert!(
            store.dump_memories_all().await.unwrap().is_empty(),
            "rows staged before the conflict must be rolled back"
        );
        assert_eq!(store.dump_learn_runs_all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn import_rejects_zip_slip_and_unknown_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let roster = scan_registry(dir.path(), "companions");
        let shared = dir.path().join("shared");

        let evil = dir.path().join("evil.zip");
        write_test_zip(
            &evil,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
                ("../evil.jsonl", "escaped"),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &evil).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert!(!dir.path().join("evil.jsonl").exists());

        let exe = dir.path().join("exe.zip");
        write_test_zip(
            &exe,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
                ("events/payload.exe", "MZ"),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &exe).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");

        let stray = dir.path().join("stray.zip");
        write_test_zip(
            &stray,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", ""),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
                ("extra.txt", "?"),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &stray).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert_eq!(store.dump_memories_all().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn memory_import_rejects_corrupt_lines_before_writing() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let roster = scan_registry(dir.path(), "companions");
        let good = serde_json::to_string(&raw_memory(&memory_fixture(20), "knowledge", "好行", "active")).unwrap();

        let corrupt = dir.path().join("corrupt.zip");
        write_test_zip(
            &corrupt,
            &[
                ("manifest.json", &manifest_json(3, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", &format!("{good}\n{{broken json\n")),
                ("learn_runs.jsonl", ""),
                ("state.json", r#"{"mood":null}"#),
            ],
        );
        let err = import_bundle(&store, &roster, &dir.path().join("shared"), &corrupt)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("第 2 行"), "{err}");
        assert_eq!(
            store.dump_memories_all().await.unwrap().len(),
            0,
            "a corrupt package must not leave a partial import"
        );
    }

    #[test]
    fn dedup_name_picks_first_free_suffix() {
        let mut existing = HashSet::new();
        assert_eq!(dedup_name(&existing, "宠"), "宠");
        existing.insert("宠".to_owned());
        assert_eq!(dedup_name(&existing, "宠"), "宠 (2)");
        existing.insert("宠 (2)".to_owned());
        existing.insert("宠 (3)".to_owned());
        assert_eq!(dedup_name(&existing, "宠"), "宠 (4)");
    }

    #[test]
    fn safe_zip_entry_path_policy() {
        assert!(safe_zip_entry_path("events/a.jsonl").is_ok());
        assert!(safe_zip_entry_path("./state.json").is_ok());
        assert!(safe_zip_entry_path("../evil.jsonl").is_err());
        assert!(safe_zip_entry_path("events/../../evil.jsonl").is_err());
        assert!(safe_zip_entry_path("/abs.jsonl").is_err());
        assert!(safe_zip_entry_path("events\\win.jsonl").is_err());
        assert!(safe_zip_entry_path("").is_err());
        assert!(safe_zip_entry_path("C:/evil.jsonl").is_err());
    }
}
