//! Append-only audit log for "外呼员工 / outbound-employee" (`PublicService`)
//! companion activity. One JSONL file per companion at
//! `{companions_dir}/{companion_id}/audit.jsonl`, living next to the companion's
//! `config.json`. Deliberately a flat file (not a DB table) so the feature ships
//! with no SQLite migration.
//!
//! Records are append-only and best-effort: a failing audit write must NEVER
//! break the turn or the exposure flip it documents. On read we return the most
//! recent `limit` entries, newest-first; the file is compacted (keeping the last
//! [`KEEP_LINES`] lines) once it grows past [`MAX_BYTES`] so it can never grow
//! unbounded.

use std::path::{Path, PathBuf};

use nomifun_api_types::ExposureMode;
use nomifun_common::{generate_prefixed_id, now_ms};
use serde::{Deserialize, Serialize};

/// Per-companion audit file name (sibling of `config.json`).
const AUDIT_FILE: &str = "audit.jsonl";
/// `detail` (truncated user text) hard cap, in chars.
const MAX_DETAIL_CHARS: usize = 200;
/// Above this on-disk size, the file is compacted to [`KEEP_LINES`] on the next
/// append (cheap `metadata().len()` check; no per-append line count).
const MAX_BYTES: u64 = 512 * 1024;
/// How many most-recent lines a compaction keeps.
const KEEP_LINES: usize = 1000;

/// One append-only audit record. Field names/types are a PINNED wire contract
/// (the frontend defines a matching type); do not rename or retype.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique per entry (`audit_<uuidv7>`).
    pub id: String,
    /// Epoch milliseconds ([`now_ms`]).
    pub at: i64,
    /// Origin surface: `"channel"` | `"desktop"` | `"remote"`.
    pub surface: String,
    /// IM platform for `surface == "channel"` (e.g. `"telegram"`); `None` otherwise.
    pub channel_platform: Option<String>,
    /// `"turn"` | `"exposure_change"`.
    pub kind: String,
    /// For `turn`: the truncated user text (≤200 chars). For `exposure_change`:
    /// `"{old} → {new}"` (e.g. `"private → public_service"`).
    pub detail: String,
}

/// A page of audit entries, most-recent-first. PINNED wire contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
}

impl AuditEntry {
    fn new(surface: &str, channel_platform: Option<String>, kind: &str, detail: String) -> Self {
        Self {
            id: generate_prefixed_id("audit"),
            at: now_ms(),
            surface: surface.to_owned(),
            channel_platform,
            kind: kind.to_owned(),
            detail,
        }
    }

    /// A `"turn"` record: an inbound turn hosted by a public-service companion.
    /// `text` is truncated to ≤200 chars.
    pub fn turn(surface: &str, channel_platform: Option<String>, text: &str) -> Self {
        Self::new(surface, channel_platform, "turn", truncate_detail(text))
    }

    /// An `"exposure_change"` record: the owner flipped a companion's exposure
    /// tier (surface = `"desktop"`, detail = `"{old} → {new}"`).
    pub fn exposure_change(old: ExposureMode, new: ExposureMode) -> Self {
        let detail = format!("{} → {}", exposure_wire(old), exposure_wire(new));
        Self::new("desktop", None, "exposure_change", detail)
    }
}

/// The snake_case wire token for an exposure tier (matches the `serde`
/// `rename_all = "snake_case"` on [`ExposureMode`]). Kept as an explicit match so
/// the audit detail can never silently diverge from the JSON representation.
fn exposure_wire(mode: ExposureMode) -> &'static str {
    match mode {
        ExposureMode::Private => "private",
        ExposureMode::TrustedRemote => "trusted_remote",
        ExposureMode::PublicService => "public_service",
    }
}

/// Take at most [`MAX_DETAIL_CHARS`] chars (by char, not byte, so multi-byte
/// text is never split mid-codepoint). No ellipsis, to stay strictly ≤200.
fn truncate_detail(s: &str) -> String {
    if s.chars().count() <= MAX_DETAIL_CHARS {
        s.to_owned()
    } else {
        s.chars().take(MAX_DETAIL_CHARS).collect()
    }
}

fn audit_path(companion_dir: &Path) -> PathBuf {
    companion_dir.join(AUDIT_FILE)
}

/// Append one entry to `{companion_dir}/audit.jsonl` (creating the dir/file).
/// A single `write_all` of one line is atomic on local filesystems. After the
/// write, the file is compacted if it has grown past [`MAX_BYTES`]. Best-effort:
/// callers ignore the error (audit must not fail the operation it records).
pub fn append_audit(companion_dir: &Path, entry: &AuditEntry) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(companion_dir)?;
    let path = audit_path(companion_dir);
    let mut line = serde_json::to_string(entry).expect("AuditEntry serializes");
    line.push('\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(line.as_bytes())?;
    // Best-effort growth cap: a compaction failure must not fail the append.
    maybe_compact(companion_dir, &path);
    Ok(())
}

/// Rewrite the file to its last [`KEEP_LINES`] lines when it exceeds
/// [`MAX_BYTES`]. Uses the crate's atomic temp+rename writer so a crash mid-
/// compaction can never truncate the live file. Silently no-ops on any error.
fn maybe_compact(companion_dir: &Path, path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() <= MAX_BYTES {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= KEEP_LINES {
        return;
    }
    let mut out = lines[lines.len() - KEEP_LINES..].join("\n");
    out.push('\n');
    let _ = crate::fsio::save_bytes_atomic(companion_dir, AUDIT_FILE, out.as_bytes());
}

/// Read the most-recent `limit` entries from `{companion_dir}/audit.jsonl`,
/// newest-first. Missing file / unreadable lines degrade to what can be parsed
/// (never an error). Corrupt lines are skipped.
pub fn read_recent_audit(companion_dir: &Path, limit: usize) -> Vec<AuditEntry> {
    let Ok(raw) = std::fs::read_to_string(audit_path(companion_dir)) else {
        return Vec::new();
    };
    let mut entries: Vec<AuditEntry> = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
        .collect();
    entries.reverse(); // append order is chronological → reverse for newest-first
    entries.truncate(limit);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        append_audit(dir.path(), &AuditEntry::turn("channel", Some("telegram".into()), "第一条")).unwrap();
        append_audit(dir.path(), &AuditEntry::turn("channel", Some("telegram".into()), "第二条")).unwrap();

        let page = read_recent_audit(dir.path(), 50);
        assert_eq!(page.len(), 2);
        // Most-recent-first: the second append comes back first.
        assert_eq!(page[0].detail, "第二条");
        assert_eq!(page[1].detail, "第一条");
        assert_eq!(page[0].kind, "turn");
        assert_eq!(page[0].surface, "channel");
        assert_eq!(page[0].channel_platform.as_deref(), Some("telegram"));
        assert!(!page[0].id.is_empty() && page[0].id != page[1].id, "each entry has a unique id");
        assert!(page[0].at >= page[1].at, "at is monotonic with append order");
    }

    #[test]
    fn read_limit_clamps_to_newest() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            append_audit(dir.path(), &AuditEntry::turn("channel", None, &format!("m{i}"))).unwrap();
        }
        let page = read_recent_audit(dir.path(), 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].detail, "m4");
        assert_eq!(page[1].detail, "m3");
        // Missing file → empty, never an error.
        assert!(read_recent_audit(&dir.path().join("nope"), 10).is_empty());
    }

    #[test]
    fn exposure_change_detail_uses_wire_tokens() {
        let e = AuditEntry::exposure_change(ExposureMode::Private, ExposureMode::PublicService);
        assert_eq!(e.kind, "exposure_change");
        assert_eq!(e.surface, "desktop");
        assert_eq!(e.channel_platform, None);
        assert_eq!(e.detail, "private → public_service");
    }

    #[test]
    fn turn_detail_truncated_to_200_chars() {
        let long = "啊".repeat(500);
        let e = AuditEntry::turn("channel", None, &long);
        assert_eq!(e.detail.chars().count(), MAX_DETAIL_CHARS);
    }

    #[test]
    fn entry_json_field_names_match_contract() {
        let e = AuditEntry::turn("channel", Some("telegram".into()), "hi");
        let v = serde_json::to_value(&e).unwrap();
        // Exactly the pinned field set.
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["at", "channel_platform", "detail", "id", "kind", "surface"]);
        assert!(v["at"].is_i64());
        assert!(v["id"].is_string());
        assert_eq!(v["channel_platform"], serde_json::json!("telegram"));
        // None serializes as JSON null (the field is always present).
        let none = AuditEntry::turn("desktop", None, "x");
        assert!(serde_json::to_value(&none).unwrap()["channel_platform"].is_null());
    }
}
