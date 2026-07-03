//! Append-only, **day-partitioned** conversation audit for public companions.
//! One JSONL file per UTC day at `public-agents/{id}/audit/{day_index}.jsonl`,
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

use nomifun_common::{generate_prefixed_id, now_ms};
use serde::{Deserialize, Serialize};

/// Sub-directory (under the agent's config dir) holding the day-files.
const AUDIT_DIR: &str = "audit";
/// `detail` truncation cap, in chars.
const MAX_DETAIL_CHARS: usize = 200;
/// Milliseconds per UTC day.
const MS_PER_DAY: i64 = 86_400_000;

/// One append-only audit record. Field names/types are a PINNED wire contract
/// (the frontend defines a matching type) — do not rename or retype.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique per entry (`audit_<uuidv7>`).
    pub id: String,
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
            id: generate_prefixed_id("audit"),
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
/// `retention_days`. Best-effort: callers ignore the error.
pub fn append(agent_dir: &Path, entry: &AuditEntry, retention_days: u32) -> std::io::Result<()> {
    use std::io::Write;
    let dir = audit_dir(agent_dir);
    std::fs::create_dir_all(&dir)?;
    let path = day_file(agent_dir, day_index(entry.at));
    let mut line = serde_json::to_string(entry).expect("AuditEntry serializes");
    line.push('\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(line.as_bytes())?;
    // Opportunistic retention: cheap (a directory listing + a few removes).
    prune(agent_dir, retention_days, day_index(entry.at));
    Ok(())
}

/// List `(day_index, path)` for every day-file present, newest day first.
fn day_files_desc(agent_dir: &Path) -> Vec<(i64, std::path::PathBuf)> {
    let mut days: Vec<(i64, std::path::PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(audit_dir(agent_dir)) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(day) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<i64>().ok()) {
                days.push((day, p));
            }
        }
    }
    days.sort_by(|a, b| b.0.cmp(&a.0));
    days
}

/// Delete day-files strictly older than `retention_days` relative to
/// `today_index` (keep the most recent `retention_days` days, today inclusive).
/// `retention_days == 0` disables pruning (keep everything). Best-effort.
fn prune(agent_dir: &Path, retention_days: u32, today_index: i64) {
    if retention_days == 0 {
        return;
    }
    let cutoff = today_index - retention_days as i64 + 1; // keep day >= cutoff
    for (day, path) in day_files_desc(agent_dir) {
        if day < cutoff {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Search / paginate the audit log, newest-first. Missing dir / corrupt lines
/// degrade gracefully (skipped), never an error.
pub fn search(agent_dir: &Path, query: &AuditQuery) -> AuditPage {
    let limit = query.limit.clamp(1, 200);
    let today = day_index(now_ms());
    let min_day = query.days.map(|d| today - d as i64 + 1);
    let q_lower = query.q.as_ref().map(|s| s.to_lowercase());

    let mut out: Vec<AuditEntry> = Vec::with_capacity(limit);
    'outer: for (day, path) in day_files_desc(agent_dir) {
        if let Some(min) = min_day {
            if day < min {
                break; // older than the window; files are sorted desc
            }
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        // Within a day-file, lines are chronological → reverse for newest-first.
        for line in raw.lines().rev() {
            let Ok(entry) = serde_json::from_str::<AuditEntry>(line) else { continue };
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
    AuditPage { entries: out, next_cursor }
}

/// Delete every day-file older than `older_than_days` (today-relative). Returns
/// the number of day-files removed. `older_than_days == 0` clears everything.
pub fn delete_older_than(agent_dir: &Path, older_than_days: u32) -> usize {
    let today = day_index(now_ms());
    let cutoff = today - older_than_days as i64 + 1;
    let mut deleted = 0;
    for (day, path) in day_files_desc(agent_dir) {
        // older_than_days==0 → cutoff = today+1 → delete all (incl. today).
        if day < cutoff && std::fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_at(at: i64, kind: &str, detail: &str) -> AuditEntry {
        AuditEntry {
            id: format!("audit_{at}_{detail}"),
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
        let page = search(d.path(), &AuditQuery { limit: 2, ..Default::default() });
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].detail, "m4");
        assert_eq!(page.entries[1].detail, "m3");
        assert_eq!(page.next_cursor, Some(base + 3));
        // Next page via cursor.
        let page2 = search(d.path(), &AuditQuery { limit: 2, cursor: page.next_cursor, ..Default::default() });
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

        let by_text = search(d.path(), &AuditQuery { limit: 50, q: Some("订单".into()), ..Default::default() });
        assert_eq!(by_text.entries.len(), 1);
        assert_eq!(by_text.entries[0].detail, "查订单");

        let by_kind = search(d.path(), &AuditQuery { limit: 50, kind: Some("exposure_change".into()), ..Default::default() });
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
        let deleted = delete_older_than(d.path(), 30);
        assert_eq!(deleted, 1);
        let remaining = search(d.path(), &AuditQuery { limit: 50, ..Default::default() });
        assert_eq!(remaining.entries.len(), 2);
        assert!(remaining.entries.iter().all(|e| e.detail != "40d"));
    }

    #[test]
    fn days_window_limits_scan() {
        let d = tempfile::tempdir().unwrap();
        let today = day_index(now_ms());
        write_raw(d.path(), &entry_at(today * MS_PER_DAY + 1, "turn", "today"));
        write_raw(d.path(), &entry_at((today - 10) * MS_PER_DAY + 1, "turn", "10d"));
        let recent = search(d.path(), &AuditQuery { limit: 50, days: Some(3), ..Default::default() });
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
        let all = search(d.path(), &AuditQuery { limit: 50, ..Default::default() });
        assert!(all.entries.iter().all(|e| e.detail != "old"), "20-day-old file pruned at retention=7");
        assert!(all.entries.iter().any(|e| e.detail == "new"));
    }
}
