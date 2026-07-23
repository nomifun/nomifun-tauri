//! Append-only, **day-partitioned** conversation audit for public companions.
//! One JSONL file per UTC day at
//! `public-agents/{public_agent_id}/audit/{day_index}.jsonl`,
//! where `day_index = at_ms / 86_400_000` (whole UTC days since epoch — no date
//! library needed, and no timezone ambiguity).
//!
//! This partitioning makes the two enterprise requirements cheap and exact:
//! - **Day-level retention / eviction**: prune whole day-files older than the
//!   agent's `audit_retention_days` (a single `remove_file` per expired day).
//! - **Search**: scan only the day-files in the requested window, newest-first,
//!   with text/kind filters and cursor pagination — never load the whole log.
//!
//! All writes are best-effort: a failing audit write must NEVER break the turn
//! (or config change) it documents.

use std::path::Path;

use nomifun_common::{AppError, PublicAgentAuditEntryId, now_ms};
use serde::{Deserialize, Serialize};

/// Sub-directory (under the agent's config dir) holding the day-files.
pub(crate) const AUDIT_DIR: &str = "audit";
/// `detail` truncation cap, in chars.
const MAX_DETAIL_CHARS: usize = 200;
/// Milliseconds per UTC day.
const MS_PER_DAY: i64 = 86_400_000;

/// One append-only audit record. Field names/types are a PINNED wire contract
/// (the frontend defines a matching type) — do not rename or retype.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    /// Unique per entry (canonical bare UUIDv7).
    pub audit_entry_id: PublicAgentAuditEntryId,
    /// Epoch milliseconds.
    pub at: i64,
    /// Origin surface: `"channel"` | `"desktop"` | `"remote"`.
    pub surface: String,
    /// IM platform for `surface == "channel"` (e.g. `"telegram"`); else `null`.
    pub channel_platform: Option<String>,
    /// `"turn"` | `"exposure_change"`.
    pub kind: String,
    /// For `turn`: truncated user text (≤200 chars). For `exposure_change`:
    /// `"{old} → {new}"`.
    pub detail: String,
}

/// A page of audit entries (most-recent-first) plus the cursor to fetch older.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    /// `at` of the last entry — pass back as `cursor` to fetch the next page.
    /// `None` when no more entries exist.
    pub next_cursor: Option<i64>,
}

/// Search / pagination parameters (all optional).
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Max entries to return (clamped 1..=200 by the caller).
    pub limit: usize,
    /// Return only entries with `at < cursor` (exclusive upper bound).
    pub cursor: Option<i64>,
    /// Case-insensitive substring filter over `detail`.
    pub q: Option<String>,
    /// Exact `kind` filter (`turn` / `exposure_change`).
    pub kind: Option<String>,
    /// Only entries within the last `days` UTC days (None = all retained).
    pub days: Option<u32>,
}

impl AuditEntry {
    fn new(surface: &str, channel_platform: Option<String>, kind: &str, detail: String) -> Self {
        Self {
            audit_entry_id: PublicAgentAuditEntryId::new(),
            at: now_ms(),
            surface: surface.to_owned(),
            channel_platform,
            kind: kind.to_owned(),
            detail,
        }
    }

    /// A `"turn"` record: an inbound turn served by this public companion.
    pub fn turn(surface: &str, channel_platform: Option<String>, text: &str) -> Self {
        Self::new(surface, channel_platform, "turn", truncate_detail(text))
    }

    /// An `"exposure_change"` / lifecycle record (surface = `"desktop"`).
    pub fn event(kind: &str, detail: impl Into<String>) -> Self {
        Self::new("desktop", None, kind, detail.into())
    }
}

fn truncate_detail(s: &str) -> String {
    if s.chars().count() <= MAX_DETAIL_CHARS {
        s.to_owned()
    } else {
        s.chars().take(MAX_DETAIL_CHARS).collect()
    }
}

fn day_index(at_ms: i64) -> i64 {
    at_ms.div_euclid(MS_PER_DAY)
}

fn audit_dir(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join(AUDIT_DIR)
}

fn day_file(agent_dir: &Path, day: i64) -> std::path::PathBuf {
    audit_dir(agent_dir).join(format!("{day}.jsonl"))
}

/// Append one entry to today's day-file, then prune day-files older than
/// `retention_days`. Retention failures are returned to the caller; the
/// already-appended record remains durable and callers may surface the
/// warning without losing the documented event.
pub fn append(agent_dir: &Path, entry: &AuditEntry, retention_days: u32) -> std::io::Result<()> {
    use std::io::Write;
    let dir = audit_dir(agent_dir);
    std::fs::create_dir_all(&dir)?;
    let path = day_file(agent_dir, day_index(entry.at));
    let created = !path.exists();
    let mut line = serde_json::to_string(entry)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    if created {
        crate::fsio::sync_dir(&dir)?;
    }
    prune(agent_dir, retention_days, day_index(entry.at))?;
    Ok(())
}

/// List `(day_index, path)` for every day-file present, newest day first.
fn day_files_desc(agent_dir: &Path) -> Result<Vec<(i64, std::path::PathBuf)>, AppError> {
    let mut days: Vec<(i64, std::path::PathBuf)> = Vec::new();
    let dir = audit_dir(agent_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(days),
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read public-agent audit directory {}: {error}",
                dir.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::Internal(format!(
                "scan public-agent audit directory {}: {error}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            AppError::Internal(format!(
                "inspect public-agent audit entry {}: {error}",
                path.display()
            ))
        })?;
        if !file_type.is_file() {
            return Err(AppError::Internal(format!(
                "public-agent audit directory contains non-regular entry {}",
                path.display()
            )));
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            return Err(AppError::Internal(format!(
                "public-agent audit directory contains unsupported entry {}",
                path.display()
            )));
        }
        let day = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "public-agent audit file has a non-UTF8 name: {}",
                    path.display()
                ))
            })?
            .parse::<i64>()
            .map_err(|error| {
                AppError::Internal(format!(
                    "public-agent audit file has invalid day name {}: {error}",
                    path.display()
                ))
            })?;
        days.push((day, path));
    }
    days.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(days)
}

fn parse_day_file(path: &Path) -> Result<Vec<AuditEntry>, AppError> {
    let raw = std::fs::read(path).map_err(|error| {
        AppError::Internal(format!(
            "read public-agent audit file {}: {error}",
            path.display()
        ))
    })?;
    if !raw.ends_with(b"\n") {
        return Err(AppError::Internal(format!(
            "public-agent audit file {} has an incomplete final record",
            path.display()
        )));
    }
    let lines: Vec<&[u8]> = raw.split(|byte| *byte == b'\n').collect();
    let mut entries = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            if index + 1 == lines.len() && raw.ends_with(b"\n") {
                continue;
            }
            return Err(AppError::Internal(format!(
                "public-agent audit file {} contains an empty record at line {}",
                path.display(),
                index + 1
            )));
        }
        match serde_json::from_slice::<AuditEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "public-agent audit file {} is corrupt at line {}: {error}",
                    path.display(),
                    index + 1
                )));
            }
        }
    }
    Ok(entries)
}

pub(crate) fn validate_agent_audit(agent_dir: &Path) -> Result<(), AppError> {
    for (_, path) in day_files_desc(agent_dir)? {
        parse_day_file(&path)?;
    }
    Ok(())
}

/// Delete day-files strictly older than `retention_days` relative to
/// `today_index` (keep the most recent `retention_days` days, today inclusive).
/// `retention_days == 0` disables pruning (keep everything).
fn prune(agent_dir: &Path, retention_days: u32, today_index: i64) -> std::io::Result<()> {
    if retention_days == 0 {
        return Ok(());
    }
    let cutoff = today_index - retention_days as i64 + 1; // keep day >= cutoff
    let day_files = day_files_desc(agent_dir).map_err(|error| {
        std::io::Error::other(error.to_string())
    })?;
    for (day, path) in day_files {
        if day < cutoff {
            crate::fsio::remove_path_entry(&path)?;
        }
    }
    Ok(())
}

/// Search / paginate the audit log, newest-first. Any unreadable or corrupt
/// record fails closed; a partial final line is not silently discarded.
pub fn search(agent_dir: &Path, query: &AuditQuery) -> Result<AuditPage, AppError> {
    let limit = query.limit.clamp(1, 200);
    let today = day_index(now_ms());
    let min_day = query.days.map(|d| today - d as i64 + 1);
    let q_lower = query.q.as_ref().map(|s| s.to_lowercase());

    let mut out: Vec<AuditEntry> = Vec::with_capacity(limit);
    'outer: for (day, path) in day_files_desc(agent_dir)? {
        if let Some(min) = min_day {
            if day < min {
                break; // older than the window; files are sorted desc
            }
        }
        // Within a day-file, lines are chronological → reverse for newest-first.
        for entry in parse_day_file(&path)?.into_iter().rev() {
            if let Some(cursor) = query.cursor {
                if entry.at >= cursor {
                    continue;
                }
            }
            if let Some(ref kind) = query.kind {
                if &entry.kind != kind {
                    continue;
                }
            }
            if let Some(ref ql) = q_lower {
                if !entry.detail.to_lowercase().contains(ql) {
                    continue;
                }
            }
            out.push(entry);
            if out.len() >= limit {
                break 'outer;
            }
        }
    }
    let next_cursor = if out.len() >= limit { out.last().map(|e| e.at) } else { None };
    Ok(AuditPage { entries: out, next_cursor })
}

/// Delete every day-file older than `older_than_days` (today-relative). Returns
/// the number of day-files removed. `older_than_days == 0` clears everything.
pub fn delete_older_than(
    agent_dir: &Path,
    older_than_days: u32,
) -> Result<usize, AppError> {
    let today = day_index(now_ms());
    let cutoff = today - older_than_days as i64 + 1;
    let mut deleted = 0;
    for (day, path) in day_files_desc(agent_dir)? {
        // older_than_days==0 → cutoff = today+1 → delete all (incl. today).
        if day < cutoff {
            let existed = path.exists();
            crate::fsio::remove_path_entry(&path).map_err(|error| {
                AppError::Internal(format!(
                    "delete public-agent audit file {}: {error}",
                    path.display()
                ))
            })?;
            if existed {
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_at(at: i64, kind: &str, detail: &str) -> AuditEntry {
        AuditEntry {
            // Keep fixtures on the same canonical wire contract as production;
            // audit ids are durable JSONL record identities, not display keys.
            audit_entry_id: PublicAgentAuditEntryId::new(),
            at,
            surface: "channel".into(),
            channel_platform: Some("telegram".into()),
            kind: kind.into(),
            detail: detail.into(),
        }
    }

    /// Write an entry directly into its day-file (bypassing now_ms) so day-
    /// partitioning + search are deterministic in tests.
    fn write_raw(dir: &Path, e: &AuditEntry) {
        use std::io::Write;
        let d = audit_dir(dir);
        std::fs::create_dir_all(&d).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(day_file(dir, day_index(e.at)))
            .unwrap();
        writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
    }

    #[test]
    fn search_is_newest_first_and_paginates_by_cursor() {
        let d = tempfile::tempdir().unwrap();
        let base = 100 * MS_PER_DAY; // day 100
        for i in 0..5 {
            write_raw(d.path(), &entry_at(base + i, "turn", &format!("m{i}")));
        }
        let page = search(d.path(), &AuditQuery { limit: 2, ..Default::default() }).unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].detail, "m4");
        assert_eq!(page.entries[1].detail, "m3");
        assert_eq!(page.next_cursor, Some(base + 3));
        // Next page via cursor.
        let page2 = search(d.path(), &AuditQuery { limit: 2, cursor: page.next_cursor, ..Default::default() }).unwrap();
        assert_eq!(page2.entries[0].detail, "m2");
        assert_eq!(page2.entries[1].detail, "m1");
    }

    #[test]
    fn search_filters_by_text_and_kind() {
        let d = tempfile::tempdir().unwrap();
        let base = 200 * MS_PER_DAY;
        write_raw(d.path(), &entry_at(base + 1, "turn", "查订单"));
        write_raw(d.path(), &entry_at(base + 2, "turn", "退货政策"));
        write_raw(d.path(), &entry_at(base + 3, "exposure_change", "private → public_service"));

        let by_text = search(d.path(), &AuditQuery { limit: 50, q: Some("订单".into()), ..Default::default() }).unwrap();
        assert_eq!(by_text.entries.len(), 1);
        assert_eq!(by_text.entries[0].detail, "查订单");

        let by_kind = search(d.path(), &AuditQuery { limit: 50, kind: Some("exposure_change".into()), ..Default::default() }).unwrap();
        assert_eq!(by_kind.entries.len(), 1);
        assert_eq!(by_kind.entries[0].kind, "exposure_change");
    }

    #[test]
    fn delete_older_than_removes_whole_day_files() {
        let d = tempfile::tempdir().unwrap();
        let today = day_index(now_ms());
        // Entries spread across today, 5 days ago, 40 days ago.
        write_raw(d.path(), &entry_at(today * MS_PER_DAY + 1, "turn", "today"));
        write_raw(d.path(), &entry_at((today - 5) * MS_PER_DAY + 1, "turn", "5d"));
        write_raw(d.path(), &entry_at((today - 40) * MS_PER_DAY + 1, "turn", "40d"));

        // Keep last 30 days → the 40-day-old file is deleted.
        let deleted = delete_older_than(d.path(), 30).unwrap();
        assert_eq!(deleted, 1);
        let remaining = search(d.path(), &AuditQuery { limit: 50, ..Default::default() }).unwrap();
        assert_eq!(remaining.entries.len(), 2);
        assert!(remaining.entries.iter().all(|e| e.detail != "40d"));
    }

    #[test]
    fn days_window_limits_scan() {
        let d = tempfile::tempdir().unwrap();
        let today = day_index(now_ms());
        write_raw(d.path(), &entry_at(today * MS_PER_DAY + 1, "turn", "today"));
        write_raw(d.path(), &entry_at((today - 10) * MS_PER_DAY + 1, "turn", "10d"));
        let recent = search(d.path(), &AuditQuery { limit: 50, days: Some(3), ..Default::default() }).unwrap();
        assert_eq!(recent.entries.len(), 1);
        assert_eq!(recent.entries[0].detail, "today");
    }

    #[test]
    fn append_prunes_beyond_retention() {
        let d = tempfile::tempdir().unwrap();
        let today = day_index(now_ms());
        // Seed an old day-file directly, then append today with retention=7.
        write_raw(d.path(), &entry_at((today - 20) * MS_PER_DAY + 1, "turn", "old"));
        append(d.path(), &AuditEntry::turn("channel", None, "new"), 7).unwrap();
        let all = search(d.path(), &AuditQuery { limit: 50, ..Default::default() }).unwrap();
        assert!(all.entries.iter().all(|e| e.detail != "old"), "20-day-old file pruned at retention=7");
        assert!(all.entries.iter().any(|e| e.detail == "new"));
    }

    #[test]
    fn newly_minted_audit_entry_has_a_canonical_durable_id() {
        let entry = AuditEntry::turn("desktop", None, "hello");
        assert!(nomifun_common::validate_uuidv7(entry.audit_entry_id.as_str()).is_ok());
        let wire = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            wire["audit_entry_id"],
            serde_json::json!(entry.audit_entry_id)
        );
        assert!(wire.get("id").is_none());
    }

    #[test]
    fn audit_entry_rejects_removed_generic_id_field() {
        let entry = AuditEntry::turn("desktop", None, "hello");
        let mut wire = serde_json::to_value(&entry).unwrap();
        wire["id"] = wire["audit_entry_id"].clone();
        wire.as_object_mut().unwrap().remove("audit_entry_id");
        assert!(serde_json::from_value::<AuditEntry>(wire).is_err());
    }
}
