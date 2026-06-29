//! Companion memory-bundle / companion-bundle zip export & import (spec §4.8) —
//! cross-machine migration for the shared memory hub and for single companions.
//!
//! Package layouts (zip root), enveloped by the same `manifest.json` shape as
//! the knowledge-base exporter (`nomifun-knowledge/src/export.rs`):
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
//! manifest format/kind/version gate before anything is written. Memory
//! import merges instead of replacing: active near-duplicates are skipped via
//! the store's `find_similar_active`, identical rows (same id + kind +
//! content, e.g. a re-imported archive) are skipped, and genuine cross-machine
//! id collisions get a fresh id while every other field stays untouched.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use nomifun_common::{AppError, TimestampMs, generate_prefixed_id, now_ms};
use serde::{Deserialize, Serialize};

use crate::profile::CompanionProfileConfig;
use crate::registry::CompanionRegistry;
use crate::service::CompanionService;
use crate::store::{CompanionLearnRun, CompanionMemory, CompanionStore};

/// `manifest.json` envelope discriminators. `version` is bumped only on
/// breaking package-layout changes; readers accept anything `<= EXPORT_VERSION`.
pub const EXPORT_FORMAT: &str = "nomifun-export";
pub const EXPORT_KIND_MEMORY: &str = "memory";
pub const EXPORT_KIND_COMPANION: &str = "companion";
pub const EXPORT_VERSION: u32 = 1;

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

#[derive(Debug, Serialize)]
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

/// `state.json` of a memory bundle. Lenient on read; mood is deliberately
/// never applied on import (the local machine's mood wins).
#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryStatePayload {
    #[serde(default)]
    mood: Option<String>,
}

/// `state.json` of a companion bundle.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CompanionStatePayload {
    #[serde(default)]
    xp: i64,
}

/// `knowledge_refs.json` of a companion bundle.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KnowledgeRefsPayload {
    #[serde(default)]
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
    async fn patch_companion(&self, id: &str, patch: serde_json::Value) -> Result<CompanionProfileConfig, AppError>;
    async fn remove_companion(&self, id: &str) -> Result<(), AppError>;
}

#[async_trait::async_trait]
impl CompanionRoster for CompanionService {
    async fn list_companions(&self) -> Vec<CompanionProfileConfig> {
        CompanionService::list_companions(self).await
    }
    async fn create_companion(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError> {
        CompanionService::create_companion(self, name, character).await
    }
    async fn patch_companion(&self, id: &str, patch: serde_json::Value) -> Result<CompanionProfileConfig, AppError> {
        CompanionService::patch_companion(self, id, patch).await
    }
    async fn remove_companion(&self, id: &str) -> Result<(), AppError> {
        CompanionService::delete_companion(self, id).await
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
    async fn patch_companion(&self, id: &str, patch: serde_json::Value) -> Result<CompanionProfileConfig, AppError> {
        self.patch(id, patch).await
    }
    async fn remove_companion(&self, id: &str) -> Result<(), AppError> {
        self.remove(id).await.map(|_| ())
    }
}

// ── Export ──────────────────────────────────────────────────────────

/// Package the shared memory hub (memories + learn runs + mood, optionally
/// the raw event day files) into a zip at `dest_path`, written atomically via
/// `{dest}.tmp` + rename.
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
            total_bytes += add_json_entry(zip, "state.json", &MemoryStatePayload { mood })?;
            let mut event_files = 0u64;
            if let Some(events_dir) = events_dir {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&events_dir)
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "jsonl"))
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort();
                for path in files {
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
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
    let xp = store.get_companion_state_i64(&profile.id, "xp").await?;

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

/// Atomic zip write: parent dirs created, payload written to `{dest}.tmp`,
/// renamed into place only on success (a failed export never leaves a
/// half-written package behind).
fn atomic_zip<T>(
    dest: &Path,
    write: impl FnOnce(&mut zip::ZipWriter<std::fs::File>) -> Result<T, AppError>,
) -> Result<T, AppError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Internal(format!("failed to create export dir: {e}")))?;
    }
    let mut tmp_name = dest.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let result = (|| {
        let file = std::fs::File::create(&tmp)
            .map_err(|e| AppError::Internal(format!("failed to create export file: {e}")))?;
        let mut zip = zip::ZipWriter::new(file);
        let out = write(&mut zip)?;
        zip.finish()
            .map_err(|e| AppError::Internal(format!("failed to write zip: {e}")))?;
        Ok(out)
    })();
    match result {
        Ok(out) => {
            if let Err(e) = std::fs::rename(&tmp, dest) {
                let _ = std::fs::remove_file(&tmp);
                return Err(AppError::Internal(format!("failed to finalize export file: {e}")));
            }
            Ok(out)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
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
/// fully *before* the first insert so a corrupt line never leaves a partial
/// import behind. The packaged mood is deliberately ignored.
async fn import_memory_bundle(
    store: &CompanionStore,
    shared_dir: &Path,
    extract_dir: &Path,
) -> Result<ImportOutcome, AppError> {
    let memories = parse_jsonl::<CompanionMemory>(&extract_dir.join("memories.jsonl"), "memories.jsonl", true)?;
    let learn_runs = parse_jsonl::<CompanionLearnRun>(&extract_dir.join("learn_runs.jsonl"), "learn_runs.jsonl", false)?;

    let mut imported = 0u64;
    let mut skipped = 0u64;
    for mut mem in memories {
        // Near-duplicate of an active local memory → skip.
        if store.find_similar_active(&mem.kind, &mem.content).await?.is_some() {
            skipped += 1;
            continue;
        }
        if let Some(existing) = store.get_memory(&mem.id).await? {
            if existing.kind == mem.kind && existing.content == mem.content {
                // The very same row (e.g. an archived memory on re-import,
                // which find_similar_active does not see) → skip, so a second
                // import of the same package never doubles anything.
                skipped += 1;
                continue;
            }
            // Genuine cross-machine id collision: fresh id, everything else
            // verbatim.
            mem.id = generate_prefixed_id("mem");
        }
        store.insert_memory_raw(&mem).await?;
        imported += 1;
    }

    for run in learn_runs {
        if store.learn_run_exists(&run.id).await? {
            continue;
        }
        store.insert_learn_run(&run).await?;
    }

    // Event day files: land only files the local machine does not have —
    // existing local files always win.
    let pkg_events = extract_dir.join("events");
    if pkg_events.is_dir() {
        let dest_dir = shared_dir.join("events");
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| AppError::Internal(format!("failed to create events dir: {e}")))?;
        if let Ok(entries) = std::fs::read_dir(&pkg_events) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() || path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let Some(name) = path.file_name() else { continue };
                let target = dest_dir.join(name);
                if target.exists() {
                    continue;
                }
                std::fs::copy(&path, &target)
                    .map_err(|e| AppError::Internal(format!("failed to place event file: {e}")))?;
            }
        }
    }

    Ok(ImportOutcome::Memory {
        imported,
        skipped_duplicates: skipped,
    })
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
    // Lenient (like the knowledge importer's meta.json): a hand-edited or
    // missing state/refs file still imports.
    let state: CompanionStatePayload = read_json_lenient(&extract_dir.join("state.json"));
    let refs: KnowledgeRefsPayload = read_json_lenient(&extract_dir.join("knowledge_refs.json"));

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
                &created.id,
                serde_json::json!({
                    "persona": profile.persona,
                    "model": profile.model,
                    "appearance": profile.appearance,
                }),
            )
            .await?;
        if state.xp != 0 {
            store.set_companion_state(&created.id, "xp", &state.xp.to_string()).await?;
        }
        Ok::<(), AppError>(())
    }
    .await;
    if let Err(e) = setup {
        // Roll back the half-imported companion; a failed rollback only warns.
        if let Err(del) = roster.remove_companion(&created.id).await {
            tracing::warn!(companion_id = %created.id, error = %del, "rollback of failed companion import left a stale companion");
        }
        return Err(e);
    }

    Ok(ImportOutcome::Companion {
        companion_id: created.id,
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
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) if !required => return Ok(Vec::new()),
        Err(_) => return Err(AppError::BadRequest(format!("导出包缺少 {label}"))),
    };
    let mut rows = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: T = serde_json::from_str(line)
            .map_err(|e| AppError::BadRequest(format!("{label} 第 {} 行无法解析: {e}", index + 1)))?;
        rows.push(row);
    }
    Ok(rows)
}

/// Lenient JSON read: missing or corrupt files fall back to `Default`.
fn read_json_lenient<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| AppError::BadRequest(format!("corrupt zip archive: {e}")))?;
        let entry_name = entry.name().to_string();
        reject_zip_symlink(&entry, &entry_name)?;
        let rel = safe_zip_entry_path(&entry_name)?;

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
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| AppError::BadRequest("不是 NomiFun 导出包".into()))?;
    validate_manifest(&manifest)
}

/// Envelope check. Parsed as loose JSON so future manifests with extra
/// fields still pass — only `format`/`kind`/`version` are load-bearing.
/// Returns the manifest `kind`.
fn validate_manifest(manifest: &serde_json::Value) -> Result<String, AppError> {
    let format = manifest.get("format").and_then(|v| v.as_str());
    if format != Some(EXPORT_FORMAT) {
        return Err(AppError::BadRequest("不是 NomiFun 导出包".into()));
    }
    let version = manifest.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version > u64::from(EXPORT_VERSION) {
        return Err(AppError::BadRequest("导入包版本过新，请升级应用".into()));
    }
    match manifest.get("kind").and_then(|v| v.as_str()) {
        Some(kind) if kind == EXPORT_KIND_MEMORY || kind == EXPORT_KIND_COMPANION => Ok(kind.to_owned()),
        Some(kind) => Err(AppError::BadRequest(format!("导入包类型不支持: {kind}"))),
        None => Err(AppError::BadRequest("不是 NomiFun 导出包".into())),
    }
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

    /// Registry over `{root}/{companions}` with its seq-watermark state beside it
    /// at `{root}/{companions}-shared` (each test roster gets its own watermark).
    fn scan_registry(root: &Path, companions: &str) -> CompanionRegistry {
        CompanionRegistry::scan(root.join(companions), root.join(format!("{companions}-shared")))
    }

    fn raw_memory(id: &str, kind: &str, content: &str, status: &str) -> CompanionMemory {
        CompanionMemory {
            id: id.to_owned(),
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
            scope_companion_id: String::new(),
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
        memories.sort_by(|a, b| a.id.cmp(&b.id));
        serde_json::to_value(&*memories).unwrap()
    }

    #[tokio::test]
    async fn memory_bundle_roundtrip_full_fidelity_and_dedup() {
        let dir = tempfile::TempDir::new().unwrap();
        let shared_a = dir.path().join("shared-a");
        std::fs::create_dir_all(shared_a.join("events")).unwrap();
        let event_line = r#"{"ts":1,"source":"chat","name":"x","data":{}}"#;
        std::fs::write(shared_a.join("events").join("2026-06-01.jsonl"), format!("{event_line}\n")).unwrap();

        let store_a = CompanionStore::open_memory().await.unwrap();
        let mut originals = vec![
            raw_memory("mem_aaa", "preference", "主人喜欢深色主题", "active"),
            raw_memory("mem_bbb", "episode", "上周修了导出 bug", "archived"),
            raw_memory("mem_ccc", "knowledge", "cargo test -p nomifun-companion 是门禁", "active"),
        ];
        for m in &originals {
            store_a.insert_memory_raw(m).await.unwrap();
        }
        store_a.set_state("mood", "happy").await.unwrap();
        store_a
            .insert_learn_run(&CompanionLearnRun {
                id: "run_1".into(),
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
        assert!(store_b.learn_run_exists("run_1").await.unwrap());
        let landed = shared_b.join("events").join("2026-06-01.jsonl");
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), format!("{event_line}\n"));

        // Re-import: everything (incl. the archived row) is skipped, the
        // tampered local event file is never overwritten.
        std::fs::write(&landed, "local edit\n").unwrap();
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
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), "local edit\n");
    }

    #[tokio::test]
    async fn memory_import_regenerates_id_on_genuine_collision() {
        let dir = tempfile::TempDir::new().unwrap();
        let shared_a = dir.path().join("shared-a");
        let store_a = CompanionStore::open_memory().await.unwrap();
        store_a
            .insert_memory_raw(&raw_memory("mem_clash", "knowledge", "来自源机器的知识", "active"))
            .await
            .unwrap();
        let zip_path = dir.path().join("clash.zip");
        export_memory_bundle(&store_a, &shared_a, &zip_path, false).await.unwrap();

        // Target machine already owns mem_clash with different content.
        let store_b = CompanionStore::open_memory().await.unwrap();
        store_b
            .insert_memory_raw(&raw_memory("mem_clash", "knowledge", "本机完全不同的知识", "active"))
            .await
            .unwrap();
        let roster_b = scan_registry(dir.path(), "companions-b");
        let outcome = import_bundle(&store_b, &roster_b, &dir.path().join("shared-b"), &zip_path)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ImportOutcome::Memory {
                imported: 1,
                skipped_duplicates: 0
            }
        );
        let all = store_b.dump_memories_all().await.unwrap();
        assert_eq!(all.len(), 2);
        let imported = all.iter().find(|m| m.content == "来自源机器的知识").unwrap();
        assert_ne!(imported.id, "mem_clash", "collision must mint a fresh id");
        assert_eq!(imported.created_at, 1_111, "other fields stay verbatim");
    }

    #[tokio::test]
    async fn companion_bundle_roundtrip_keeps_xp_suffixes_name_and_echoes_refs() {
        let dir = tempfile::TempDir::new().unwrap();
        let store_a = CompanionStore::open_memory().await.unwrap();
        let reg_a = scan_registry(dir.path(), "companions-a");
        let created = reg_a.create("毛球", "ink").await.unwrap();
        let profile = reg_a
            .patch(
                &created.id,
                serde_json::json!({
                    "persona": {"preset": "sassy", "custom": "喜欢用颜文字"},
                    "model": {"provider_id": "prov_x", "model": "claude-fable-5"},
                    "appearance": {"companion_enabled": true, "companion_x": 7, "quiet_start": "22:00", "quiet_end": "08:00"}
                }),
            )
            .await
            .unwrap();
        store_a.add_companion_xp(&profile.id, 57).await.unwrap();

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
        assert_ne!(companion_id, profile.id, "imported companion gets a fresh id");

        let imported = reg_b.get(&companion_id).await.unwrap();
        assert_eq!(imported.name, "毛球 (2)");
        // A fresh local short number is allocated (the bundle's own seq is
        // ignored): "毛球" took 1 on this roster, so the import gets 2.
        assert_eq!(imported.seq, Some(2));
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
    async fn import_rejects_wrong_format_kind_newer_version_and_garbage() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CompanionStore::open_memory().await.unwrap();
        let roster = scan_registry(dir.path(), "companions");
        let shared = dir.path().join("shared");

        let wrong_format = dir.path().join("format.zip");
        write_test_zip(
            &wrong_format,
            &[(
                "manifest.json",
                r#"{"format":"other-export","version":1,"kind":"memory"}"#,
            )],
        );
        let err = import_bundle(&store, &roster, &shared, &wrong_format).await.unwrap_err();
        assert!(err.to_string().contains("不是 NomiFun 导出包"), "{err}");

        let wrong_kind = dir.path().join("kind.zip");
        write_test_zip(&wrong_kind, &[("manifest.json", &manifest_json(1, "knowledge-base"))]);
        let err = import_bundle(&store, &roster, &shared, &wrong_kind).await.unwrap_err();
        assert!(err.to_string().contains("导入包类型不支持"), "{err}");

        let too_new = dir.path().join("future.zip");
        write_test_zip(&too_new, &[("manifest.json", &manifest_json(2, EXPORT_KIND_MEMORY))]);
        let err = import_bundle(&store, &roster, &shared, &too_new).await.unwrap_err();
        assert!(err.to_string().contains("导入包版本过新"), "{err}");

        let not_zip = dir.path().join("garbage.zip");
        std::fs::write(&not_zip, "definitely not a zip").unwrap();
        let err = import_bundle(&store, &roster, &shared, &not_zip).await.unwrap_err();
        assert!(err.to_string().contains("不是 NomiFun 导出包"), "{err}");

        let missing = dir.path().join("missing.zip");
        let err = import_bundle(&store, &roster, &shared, &missing).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");

        // A memory package without memories.jsonl is rejected explicitly.
        let incomplete = dir.path().join("incomplete.zip");
        write_test_zip(&incomplete, &[("manifest.json", &manifest_json(1, EXPORT_KIND_MEMORY))]);
        let err = import_bundle(&store, &roster, &shared, &incomplete).await.unwrap_err();
        assert!(err.to_string().contains("memories.jsonl"), "{err}");
        assert_eq!(store.dump_memories_all().await.unwrap().len(), 0);
        assert!(roster.list().await.is_empty());
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
                ("manifest.json", &manifest_json(1, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", ""),
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
                ("manifest.json", &manifest_json(1, EXPORT_KIND_MEMORY)),
                ("events/payload.exe", "MZ"),
            ],
        );
        let err = import_bundle(&store, &roster, &shared, &exe).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");

        let stray = dir.path().join("stray.zip");
        write_test_zip(
            &stray,
            &[
                ("manifest.json", &manifest_json(1, EXPORT_KIND_MEMORY)),
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
        let good = serde_json::to_string(&raw_memory("mem_ok", "knowledge", "好行", "active")).unwrap();

        let corrupt = dir.path().join("corrupt.zip");
        write_test_zip(
            &corrupt,
            &[
                ("manifest.json", &manifest_json(1, EXPORT_KIND_MEMORY)),
                ("memories.jsonl", &format!("{good}\n{{broken json\n")),
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
