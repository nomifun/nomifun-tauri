//! The companion's dedicated sqlite store (`{companion_dir}/memory.db`): memories,
//! suggestions, companion-chat history, learn-run history, and a small
//! key-value state table (xp/mood/cursor/rolling chat summary).
//!
//! Deliberately a separate db file from the main app database so "clear all
//! companion data" stays a file-scoped operation and companion writes never contend with
//! conversation traffic.

use std::path::{Path, PathBuf};

use nomifun_common::{
    AppError, CompanionId, CompanionLearnRunId, CompanionMemoryId,
    CompanionSessionWindowId, CompanionSuggestionId, ConversationId, TimestampMs,
    now_ms, validate_uuidv7,
};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

/// Memory kinds — the six-dimension taxonomy from the design doc.
pub const MEMORY_KINDS: [&str; 6] = ["profile", "preference", "knowledge", "episode", "task", "affective"];

/// Per-kind decay half-life in days. `profile` does not decay.
fn half_life_days(kind: &str) -> Option<f64> {
    match kind {
        "episode" => Some(7.0),
        "task" => Some(14.0),
        "affective" => Some(21.0),
        "knowledge" | "preference" => Some(60.0),
        _ => None, // profile
    }
}

/// Below this strength a memory is auto-archived (still restorable in the UI).
const ARCHIVE_THRESHOLD: f64 = 0.05;

/// Visibility of a companion memory. Mirrors the companion-skills scoping
/// (`scope_kind` `'user'`=shared / `'companion'`=private + `scope_companion_id`).
/// Shared memories inject/recall for every companion; private
/// memories only for their owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryScope {
    /// Cross-companion: visible to every companion.
    Shared,
    /// Owned by one companion: visible only to it.
    Companion(String),
}

impl MemoryScope {
    /// `(scope_kind, scope_companion_id)` column values.
    pub fn columns(&self) -> Result<(&'static str, Option<String>), AppError> {
        match self {
            MemoryScope::Shared => Ok(("user", None)),
            MemoryScope::Companion(id) => {
                validate_companion_id(id, "memory scope companion_id")?;
                Ok(("companion", Some(id.clone())))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionMemory {
    pub memory_id: String,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: f64,
    pub strength: f64,
    pub pinned: bool,
    pub source: String,
    pub status: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub last_reinforced_at: TimestampMs,
    /// `'user'` = shared (all companions) / `'companion'` = private to one.
    pub scope_kind: String,
    /// Owning canonical companion id when private; `None` when shared.
    pub scope_companion_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionSuggestion {
    pub suggestion_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    /// Optional UI action, e.g. `{"type":"navigate","to":"/scheduled"}`.
    pub action: Option<serde_json::Value>,
    pub status: String,
    pub created_at: TimestampMs,
    pub decided_at: Option<TimestampMs>,
}

/// One suggestion page and the number of rows matching the same status filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionPage {
    pub items: Vec<CompanionSuggestion>,
    pub total: i64,
}


/// One registered companion chat thread (a real `type='nomi'` conversation
/// owned by the main conversation domain; the companion only tracks membership).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionThread {
    pub conversation_id: String,
    /// Owning canonical companion UUIDv7. Ownerless rows are invalid.
    pub companion_id: String,
    pub title: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// One archived (or currently open) companion session window — a bounded span
/// of the companion's single chat thread. Closed on ≥`idle_minutes` of
/// inactivity, compressed into a day-partitioned `digest`, after which the live
/// engine context is reset (`clear_context`) so the next window starts small.
/// `session_day` is the window's LOCAL start day (`YYYYMMDD`) — the partition key
/// for "去年今日" recall, so a cross-midnight session stays attributed to the day
/// it began.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWindow {
    pub session_window_id: String,
    pub companion_id: String,
    pub conversation_id: String,
    pub session_day: String,
    pub started_at: TimestampMs,
    pub last_activity_at: TimestampMs,
    pub closed_at: Option<TimestampMs>,
    /// `open` | `archived` | `skipped` (too little content to summarize).
    pub status: String,
    pub message_count: i64,
    /// Only messages with `created_at > boundary_ts` belong to this window.
    pub boundary_ts: TimestampMs,
    pub digest: Option<String>,
    /// JSON blob of structured highlights (topics/decisions/mood/todos).
    pub highlights: Option<String>,
    pub token_estimate: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionLearnRun {
    pub learn_run_id: String,
    pub started_at: TimestampMs,
    pub finished_at: Option<TimestampMs>,
    pub status: String,
    pub events_processed: i64,
    pub memories_added: i64,
    pub suggestions_added: i64,
    pub error: Option<String>,
    /// nomi's one-line diary for this run, shown on the overview tab.
    pub summary: Option<String>,
}

/// One durable mined-pattern sample. This fixed JSON structure replaces the
/// historical delimiter-concatenated pseudo-ID representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternExample {
    conversation_id: ConversationId,
    #[serde(deserialize_with = "deserialize_uuidv7_string")]
    event_id: String,
}

fn deserialize_uuidv7_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_uuidv7(&value).map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn deserialize_uuidv7_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    for value in &values {
        validate_uuidv7(value).map_err(serde::de::Error::custom)?;
    }
    Ok(values)
}

fn deserialize_optional_uuidv7_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = value.as_deref() {
        validate_uuidv7(value).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

/// Filter for `list_memories`.
#[derive(Debug, Default, Clone)]
pub struct MemoryFilter {
    pub kind: Option<String>,
    pub q: Option<String>,
    pub status: Option<String>,
    /// When set, return only memories visible to this companion: shared
    /// (`scope_kind='user'`) plus the companion's own private ones. `None`
    /// returns every memory regardless of scope (cross-companion "all" view).
    pub scope_companion_id: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

/// One page of memories and the number of rows matching the same filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPage {
    pub items: Vec<CompanionMemory>,
    pub total: i64,
}

fn memory_filter_clause(filter: &MemoryFilter) -> String {
    let mut sql = String::from(" WHERE 1=1");
    if filter.kind.is_some() {
        sql.push_str(" AND kind = ?");
    }
    if filter.q.is_some() {
        sql.push_str(" AND content LIKE ?");
    }
    if filter.status.is_some() {
        sql.push_str(" AND status = ?");
    }
    if filter.scope_companion_id.is_some() {
        // Shared (all companions) + this companion's own private memories.
        sql.push_str(" AND (scope_kind = 'user' OR scope_companion_id = ?)");
    }
    sql
}

#[derive(Clone)]
pub struct CompanionStore {
    pool: SqlitePool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemoryImportStats {
    pub imported: u64,
    pub skipped_duplicates: u64,
}

/// An open SQLite transaction containing a fully validated memory-bundle
/// merge. The export/import layer publishes staged event files only after this
/// object has been created; it then commits the DB transaction. Dropping or
/// explicitly rolling back this value leaves the existing store unchanged.
pub(crate) struct MemoryImportTransaction<'a> {
    tx: sqlx::Transaction<'a, sqlx::Sqlite>,
    stats: MemoryImportStats,
}

impl MemoryImportTransaction<'_> {
    pub(crate) fn stats(&self) -> MemoryImportStats {
        self.stats
    }

    pub(crate) async fn commit(self) -> Result<MemoryImportStats, AppError> {
        self.tx.commit().await.map_err(db_err)?;
        Ok(self.stats)
    }

    pub(crate) async fn rollback(self) -> Result<(), AppError> {
        self.tx.rollback().await.map_err(db_err)
    }
}

impl CompanionStore {
    /// Validate and stage a complete memory-bundle merge in one SQLite
    /// transaction. No row is visible to other connections until the returned
    /// transaction is committed.
    pub(crate) async fn begin_memory_import(
        &self,
        memories: &[CompanionMemory],
        learn_runs: &[CompanionLearnRun],
    ) -> Result<MemoryImportTransaction<'_>, AppError> {
        for memory in memories {
            CompanionMemoryId::try_from(memory.memory_id.as_str())
                .map_err(|error| AppError::BadRequest(format!("invalid imported memory id: {error}")))?;
            match (memory.scope_kind.as_str(), memory.scope_companion_id.as_deref()) {
                ("user", None) => {}
                ("companion", Some(owner)) => validate_companion_id(owner, "imported memory scope companion_id")?,
                _ => {
                    return Err(AppError::BadRequest(
                        "imported memory scope must be shared (user/None) or private (companion/Some(canonical ID))".into(),
                    ));
                }
            }
        }
        for run in learn_runs {
            CompanionLearnRunId::try_from(run.learn_run_id.as_str())
                .map_err(|error| AppError::BadRequest(format!("invalid imported learn-run id: {error}")))?;
        }

        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let mut imported = 0u64;
        let mut skipped_duplicates = 0u64;

        for memory in memories {
            let existing = sqlx::query("SELECT * FROM companion_memories WHERE memory_id = ?")
                .bind(&memory.memory_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
            if let Some(row) = existing {
                let local = row_to_memory(&row)?;
                if local == *memory {
                    skipped_duplicates += 1;
                    continue;
                }
                return Err(AppError::Conflict(format!(
                    "memory import ID collision for {}: local and imported content differ",
                    memory.memory_id
                )));
            }

            if memory.status == "active" {
                let similar = sqlx::query(
                    "SELECT memory_id, content
                     FROM companion_memories
                     WHERE kind = ? AND status = 'active'",
                )
                .bind(&memory.kind)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                let normalized = memory.content.trim().to_lowercase();
                let duplicate = similar.into_iter().any(|row| {
                    let existing_content: String = row.get("content");
                    let existing_normalized = existing_content.trim().to_lowercase();
                    if existing_normalized == normalized {
                        return true;
                    }
                    let short_len = normalized.chars().count().min(existing_normalized.chars().count());
                    let long_len = normalized.chars().count().max(existing_normalized.chars().count());
                    long_len > 0
                        && (short_len as f64 / long_len as f64) >= 0.6
                        && (existing_normalized.contains(&normalized) || normalized.contains(&existing_normalized))
                });
                if duplicate {
                    skipped_duplicates += 1;
                    continue;
                }
            }

            sqlx::query(
                "INSERT INTO companion_memories(memory_id, kind, content, tags, importance, strength, pinned, source, status, created_at, updated_at, last_reinforced_at, scope_kind, scope_companion_id)
                 VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(&memory.memory_id)
            .bind(&memory.kind)
            .bind(&memory.content)
            .bind(serde_json::to_string(&memory.tags).map_err(|error| {
                AppError::BadRequest(format!("invalid imported memory tags: {error}"))
            })?)
            .bind(memory.importance)
            .bind(memory.strength)
            .bind(memory.pinned as i64)
            .bind(&memory.source)
            .bind(&memory.status)
            .bind(memory.created_at)
            .bind(memory.updated_at)
            .bind(memory.last_reinforced_at)
            .bind(&memory.scope_kind)
            .bind(&memory.scope_companion_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            imported += 1;
        }

        for run in learn_runs {
            let existing = sqlx::query("SELECT * FROM companion_learn_runs WHERE learn_run_id = ?")
                .bind(&run.learn_run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
            if let Some(row) = existing {
                let local = row_to_learn_run(&row)?;
                if local == *run {
                    continue;
                }
                return Err(AppError::Conflict(format!(
                    "learn-run import ID collision for {}: local and imported content differ",
                    run.learn_run_id
                )));
            }
            sqlx::query(
                "INSERT INTO companion_learn_runs(learn_run_id, started_at, finished_at, status, events_processed, memories_added, suggestions_added, error, summary)
                 VALUES(?,?,?,?,?,?,?,?,?)",
            )
            .bind(&run.learn_run_id)
            .bind(run.started_at)
            .bind(run.finished_at)
            .bind(&run.status)
            .bind(run.events_processed)
            .bind(run.memories_added)
            .bind(run.suggestions_added)
            .bind(&run.error)
            .bind(&run.summary)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        Ok(MemoryImportTransaction {
            tx,
            stats: MemoryImportStats {
                imported,
                skipped_duplicates,
            },
        })
    }
}

/// Boot-time registration of the live file-backed store and the shared dir
/// it was opened on. `CompanionService` keeps its store/dirs private and exposes no
/// accessor (and service.rs is owned by other workstreams), so the
/// export/import routes need a crate-visible handle to the *live* pool —
/// [`CompanionStore::open`] records it here. First-wins is correct: production
/// calls `open` exactly once (the shared `memory.db` in `CompanionService::start`);
/// tests pass their stores to the export functions explicitly and never read
/// this.
static LIVE_STORE: std::sync::OnceLock<(PathBuf, CompanionStore)> = std::sync::OnceLock::new();

/// The live file-backed store plus its shared dir, when one was opened in
/// this process. `None` means the production store has not been opened; boot
/// fails closed rather than substituting an in-memory database.
pub fn live_store() -> Option<(&'static Path, &'static CompanionStore)> {
    LIVE_STORE.get().map(|(dir, store)| (dir.as_path(), store))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS companion_memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id TEXT NOT NULL UNIQUE CHECK (
    length(memory_id) = 36
    AND lower(memory_id) = memory_id
    AND memory_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(memory_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  kind TEXT NOT NULL,
  content TEXT NOT NULL,
  tags TEXT NOT NULL DEFAULT '[]',
  importance REAL NOT NULL DEFAULT 0.5,
  strength REAL NOT NULL DEFAULT 0.5,
  pinned INTEGER NOT NULL DEFAULT 0,
  source TEXT NOT NULL DEFAULT 'learn',
  status TEXT NOT NULL DEFAULT 'active',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_reinforced_at INTEGER NOT NULL,
  scope_kind TEXT NOT NULL DEFAULT 'user' CHECK(scope_kind IN ('user', 'companion')),
  scope_companion_id TEXT CHECK (
    scope_companion_id IS NULL
    OR (
      length(scope_companion_id) = 36
      AND lower(scope_companion_id) = scope_companion_id
      AND scope_companion_id GLOB '????????-????-7???-[89ab]???-????????????'
      AND replace(scope_companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
    )
  ),
  CHECK((scope_kind = 'user' AND scope_companion_id IS NULL) OR
        (scope_kind = 'companion' AND scope_companion_id IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_companion_memories_kind ON companion_memories(kind, status, strength DESC);

CREATE TABLE IF NOT EXISTS companion_suggestions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  suggestion_id TEXT NOT NULL UNIQUE CHECK (
    length(suggestion_id) = 36
    AND lower(suggestion_id) = suggestion_id
    AND suggestion_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(suggestion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  kind TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  action TEXT,
  status TEXT NOT NULL DEFAULT 'new',
  created_at INTEGER NOT NULL,
  decided_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_companion_suggestions_status ON companion_suggestions(status, created_at DESC);

CREATE TABLE IF NOT EXISTS companion_learn_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  learn_run_id TEXT NOT NULL UNIQUE CHECK (
    length(learn_run_id) = 36
    AND lower(learn_run_id) = learn_run_id
    AND learn_run_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(learn_run_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  started_at INTEGER NOT NULL,
  finished_at INTEGER,
  status TEXT NOT NULL,
  events_processed INTEGER NOT NULL DEFAULT 0,
  memories_added INTEGER NOT NULL DEFAULT 0,
  suggestions_added INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  summary TEXT
);

CREATE TABLE IF NOT EXISTS companion_state (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  state_key TEXT NOT NULL UNIQUE,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS companion_threads (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  conversation_id TEXT NOT NULL UNIQUE CHECK (
    length(conversation_id) = 36
    AND lower(conversation_id) = conversation_id
    AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  companion_id TEXT NOT NULL UNIQUE CHECK (
    length(companion_id) = 36
    AND lower(companion_id) = companion_id
    AND companion_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  title TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS companion_runtime_state (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  companion_id TEXT NOT NULL CHECK (
    length(companion_id) = 36
    AND lower(companion_id) = companion_id
    AND companion_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  state_key TEXT NOT NULL,
  value TEXT NOT NULL,
  UNIQUE(companion_id, state_key)
);

CREATE TABLE IF NOT EXISTS companion_skills (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  companion_skill_id TEXT NOT NULL UNIQUE CHECK (
    length(companion_skill_id) = 36
    AND lower(companion_skill_id) = companion_skill_id
    AND companion_skill_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(companion_skill_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  skill_name TEXT NOT NULL,
  scope_kind TEXT NOT NULL DEFAULT 'companion' CHECK(scope_kind IN ('user', 'companion')),
  scope_companion_id TEXT CHECK (
    scope_companion_id IS NULL
    OR (
      length(scope_companion_id) = 36
      AND lower(scope_companion_id) = scope_companion_id
      AND scope_companion_id GLOB '????????-????-7???-[89ab]???-????????????'
      AND replace(scope_companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
    )
  ),
  status TEXT NOT NULL DEFAULT 'draft',
  source TEXT NOT NULL DEFAULT 'mined',
  confidence REAL NOT NULL DEFAULT 0.0,
  provenance_event_ids TEXT NOT NULL DEFAULT '[]',
  strength REAL NOT NULL DEFAULT 1.0,
  version INTEGER NOT NULL DEFAULT 1,
  skill_pattern_id TEXT CHECK (
    skill_pattern_id IS NULL OR (
      length(skill_pattern_id) = 36
      AND lower(skill_pattern_id) = skill_pattern_id
      AND skill_pattern_id GLOB '????????-????-7???-[89ab]???-????????????'
      AND replace(skill_pattern_id, '-', '') NOT GLOB '*[^0-9a-f]*'
    )
  ),
  usage_count INTEGER NOT NULL DEFAULT 0,
  last_used_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  signature TEXT NOT NULL DEFAULT '',
  CHECK((scope_kind = 'user' AND scope_companion_id IS NULL) OR
        (scope_kind = 'companion' AND scope_companion_id IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_companion_skills_owner ON companion_skills(scope_companion_id, status, strength DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_companion_skills_shared_name ON companion_skills(skill_name) WHERE scope_kind = 'user';
CREATE UNIQUE INDEX IF NOT EXISTS idx_companion_skills_private_owner_name ON companion_skills(scope_companion_id, skill_name) WHERE scope_kind = 'companion';

CREATE TABLE IF NOT EXISTS skill_pattern_stats (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  skill_pattern_id TEXT NOT NULL UNIQUE CHECK (
    length(skill_pattern_id) = 36
    AND lower(skill_pattern_id) = skill_pattern_id
    AND skill_pattern_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(skill_pattern_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  signature TEXT NOT NULL,
  occurrence_count INTEGER NOT NULL DEFAULT 0,
  distinct_sessions INTEGER NOT NULL DEFAULT 0,
  examples TEXT NOT NULL DEFAULT '[]',
  status TEXT NOT NULL DEFAULT 'open',
  last_seen INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evolution_feedback (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  feedback_id TEXT NOT NULL UNIQUE CHECK (
    length(feedback_id) = 36
    AND lower(feedback_id) = feedback_id
    AND feedback_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(feedback_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  companion_skill_id TEXT NOT NULL CHECK (
    length(companion_skill_id) = 36
    AND lower(companion_skill_id) = companion_skill_id
    AND companion_skill_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(companion_skill_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  skill_name_snapshot TEXT NOT NULL,
  skill_pattern_id TEXT CHECK (
    skill_pattern_id IS NULL OR (
      length(skill_pattern_id) = 36
      AND lower(skill_pattern_id) = skill_pattern_id
      AND skill_pattern_id GLOB '????????-????-7???-[89ab]???-????????????'
      AND replace(skill_pattern_id, '-', '') NOT GLOB '*[^0-9a-f]*'
    )
  ),
  signature_snapshot TEXT,
  decision TEXT NOT NULL,
  reason TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_pattern_signature ON skill_pattern_stats(signature);
CREATE INDEX IF NOT EXISTS idx_evolution_feedback_skill ON evolution_feedback(companion_skill_id);
CREATE INDEX IF NOT EXISTS idx_evolution_feedback_pattern ON evolution_feedback(skill_pattern_id);

CREATE TABLE IF NOT EXISTS companion_session_windows (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_window_id TEXT NOT NULL UNIQUE CHECK (
    length(session_window_id) = 36
    AND lower(session_window_id) = session_window_id
    AND session_window_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(session_window_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  companion_id TEXT NOT NULL CHECK (
    length(companion_id) = 36
    AND lower(companion_id) = companion_id
    AND companion_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(companion_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  conversation_id TEXT NOT NULL CHECK (
    length(conversation_id) = 36
    AND lower(conversation_id) = conversation_id
    AND conversation_id GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(conversation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
  ),
  session_day TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL,
  closed_at INTEGER,
  status TEXT NOT NULL DEFAULT 'open',
  message_count INTEGER NOT NULL DEFAULT 0,
  boundary_ts INTEGER NOT NULL DEFAULT 0,
  digest TEXT,
  highlights TEXT,
  token_estimate INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_csw_companion_day ON companion_session_windows(companion_id, session_day);
CREATE INDEX IF NOT EXISTS idx_csw_status ON companion_session_windows(companion_id, status, last_activity_at);
"#;

fn db_err(e: sqlx::Error) -> AppError {
    AppError::Internal(format!("companion store: {e}"))
}

fn validate_companion_id(value: &str, field: &str) -> Result<(), AppError> {
    CompanionId::try_from(value)
        .map(|_| ())
        .map_err(|error| AppError::BadRequest(format!("invalid {field}: {error}")))
}

fn validate_conversation_id(value: &str, field: &str) -> Result<(), AppError> {
    ConversationId::try_from(value)
        .map(|_| ())
        .map_err(|error| AppError::BadRequest(format!("invalid {field}: {error}")))
}

fn invalid_disk_id(field: &str, value: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal(format!(
        "companion store contains non-canonical {field} {value:?}: {error}"
    ))
}

/// Companion side-store v3 is a hard baseline. The app-level factory reset
/// removes any non-v3 dataset before this crate starts, so this crate creates
/// only the current schema and never transforms existing rows.
const STORE_VERSION: i64 = 3;

#[derive(Debug, Clone, Copy)]
struct ColumnContract {
    name: &'static str,
    declared_type: &'static str,
    not_null: bool,
    primary_key_position: i64,
}

#[derive(Debug, Clone, Copy)]
struct UniqueIndexContract {
    columns: &'static [&'static str],
    origin: &'static str,
    partial: bool,
}

#[derive(Debug, Clone, Copy)]
struct TableContract {
    name: &'static str,
    columns: &'static [ColumnContract],
    uuidv7_columns: &'static [&'static str],
    unique_indexes: &'static [UniqueIndexContract],
    required_sql_fragments: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
struct IndexColumnContract {
    name: &'static str,
    descending: bool,
}

#[derive(Debug, Clone, Copy)]
struct NamedIndexContract {
    name: &'static str,
    table: &'static str,
    unique: bool,
    partial: bool,
    columns: &'static [IndexColumnContract],
    where_fragment: Option<&'static str>,
}

const BASELINE_TABLES: &[TableContract] = &[
    TableContract {
        name: "companion_memories",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "memory_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "kind", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "content", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "tags", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "importance", declared_type: "REAL", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "strength", declared_type: "REAL", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "pinned", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "source", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "status", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "created_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "updated_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "last_reinforced_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "scope_kind", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "scope_companion_id", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
        ],
        uuidv7_columns: &["memory_id", "scope_companion_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["memory_id"], origin: "u", partial: false }],
        required_sql_fragments: &[
            "scope_kindin('user','companion')",
            "scope_kind='user'andscope_companion_idisnull",
            "scope_kind='companion'andscope_companion_idisnotnull",
        ],
    },
    TableContract {
        name: "companion_suggestions",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "suggestion_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "kind", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "title", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "body", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "action", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "status", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "created_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "decided_at", declared_type: "INTEGER", not_null: false, primary_key_position: 0 },
        ],
        uuidv7_columns: &["suggestion_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["suggestion_id"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "companion_learn_runs",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "learn_run_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "started_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "finished_at", declared_type: "INTEGER", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "status", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "events_processed", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "memories_added", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "suggestions_added", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "error", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "summary", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
        ],
        uuidv7_columns: &["learn_run_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["learn_run_id"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "companion_state",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "state_key", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "value", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &[],
        unique_indexes: &[UniqueIndexContract { columns: &["state_key"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "companion_threads",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "conversation_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "companion_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "title", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "created_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "updated_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &["conversation_id", "companion_id"],
        unique_indexes: &[
            UniqueIndexContract { columns: &["conversation_id"], origin: "u", partial: false },
            UniqueIndexContract { columns: &["companion_id"], origin: "u", partial: false },
        ],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "companion_runtime_state",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "companion_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "state_key", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "value", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &["companion_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["companion_id", "state_key"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "companion_skills",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "companion_skill_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "skill_name", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "scope_kind", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "scope_companion_id", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "status", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "source", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "confidence", declared_type: "REAL", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "provenance_event_ids", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "strength", declared_type: "REAL", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "version", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "skill_pattern_id", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "usage_count", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "last_used_at", declared_type: "INTEGER", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "created_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "updated_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "signature", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &["companion_skill_id", "scope_companion_id", "skill_pattern_id"],
        unique_indexes: &[
            UniqueIndexContract { columns: &["companion_skill_id"], origin: "u", partial: false },
            UniqueIndexContract { columns: &["skill_name"], origin: "c", partial: true },
            UniqueIndexContract { columns: &["scope_companion_id", "skill_name"], origin: "c", partial: true },
        ],
        required_sql_fragments: &[
            "scope_kindin('user','companion')",
            "scope_kind='user'andscope_companion_idisnull",
            "scope_kind='companion'andscope_companion_idisnotnull",
        ],
    },
    TableContract {
        name: "skill_pattern_stats",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "skill_pattern_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "signature", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "occurrence_count", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "distinct_sessions", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "examples", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "status", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "last_seen", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &["skill_pattern_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["skill_pattern_id"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "evolution_feedback",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "feedback_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "companion_skill_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "skill_name_snapshot", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "skill_pattern_id", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "signature_snapshot", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "decision", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "reason", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "created_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &["feedback_id", "companion_skill_id", "skill_pattern_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["feedback_id"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
    TableContract {
        name: "companion_session_windows",
        columns: &[
            ColumnContract { name: "id", declared_type: "INTEGER", not_null: false, primary_key_position: 1 },
            ColumnContract { name: "session_window_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "companion_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "conversation_id", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "session_day", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "started_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "last_activity_at", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "closed_at", declared_type: "INTEGER", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "status", declared_type: "TEXT", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "message_count", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "boundary_ts", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
            ColumnContract { name: "digest", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "highlights", declared_type: "TEXT", not_null: false, primary_key_position: 0 },
            ColumnContract { name: "token_estimate", declared_type: "INTEGER", not_null: true, primary_key_position: 0 },
        ],
        uuidv7_columns: &["session_window_id", "companion_id", "conversation_id"],
        unique_indexes: &[UniqueIndexContract { columns: &["session_window_id"], origin: "u", partial: false }],
        required_sql_fragments: &[],
    },
];

const BASELINE_INDEXES: &[NamedIndexContract] = &[
    NamedIndexContract {
        name: "idx_companion_memories_kind",
        table: "companion_memories",
        unique: false,
        partial: false,
        columns: &[
            IndexColumnContract { name: "kind", descending: false },
            IndexColumnContract { name: "status", descending: false },
            IndexColumnContract { name: "strength", descending: true },
        ],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_companion_suggestions_status",
        table: "companion_suggestions",
        unique: false,
        partial: false,
        columns: &[
            IndexColumnContract { name: "status", descending: false },
            IndexColumnContract { name: "created_at", descending: true },
        ],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_companion_skills_owner",
        table: "companion_skills",
        unique: false,
        partial: false,
        columns: &[
            IndexColumnContract { name: "scope_companion_id", descending: false },
            IndexColumnContract { name: "status", descending: false },
            IndexColumnContract { name: "strength", descending: true },
        ],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_companion_skills_shared_name",
        table: "companion_skills",
        unique: true,
        partial: true,
        columns: &[IndexColumnContract { name: "skill_name", descending: false }],
        where_fragment: Some("wherescope_kind='user'"),
    },
    NamedIndexContract {
        name: "idx_companion_skills_private_owner_name",
        table: "companion_skills",
        unique: true,
        partial: true,
        columns: &[
            IndexColumnContract { name: "scope_companion_id", descending: false },
            IndexColumnContract { name: "skill_name", descending: false },
        ],
        where_fragment: Some("wherescope_kind='companion'"),
    },
    NamedIndexContract {
        name: "idx_skill_pattern_signature",
        table: "skill_pattern_stats",
        unique: false,
        partial: false,
        columns: &[IndexColumnContract { name: "signature", descending: false }],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_evolution_feedback_skill",
        table: "evolution_feedback",
        unique: false,
        partial: false,
        columns: &[IndexColumnContract { name: "companion_skill_id", descending: false }],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_evolution_feedback_pattern",
        table: "evolution_feedback",
        unique: false,
        partial: false,
        columns: &[IndexColumnContract { name: "skill_pattern_id", descending: false }],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_csw_companion_day",
        table: "companion_session_windows",
        unique: false,
        partial: false,
        columns: &[
            IndexColumnContract { name: "companion_id", descending: false },
            IndexColumnContract { name: "session_day", descending: false },
        ],
        where_fragment: None,
    },
    NamedIndexContract {
        name: "idx_csw_status",
        table: "companion_session_windows",
        unique: false,
        partial: false,
        columns: &[
            IndexColumnContract { name: "companion_id", descending: false },
            IndexColumnContract { name: "status", descending: false },
            IndexColumnContract { name: "last_activity_at", descending: false },
        ],
        where_fragment: None,
    },
];

fn normalized_schema_sql(sql: &str) -> String {
    sql.chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

async fn index_key_columns(
    pool: &SqlitePool,
    index_name: &str,
) -> Result<Vec<(String, bool)>, AppError> {
    let rows = sqlx::query(
        "SELECT name, \"desc\" AS descending \
         FROM pragma_index_xinfo(?) WHERE \"key\" = 1 ORDER BY seqno",
    )
    .bind(index_name)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    rows.into_iter()
        .map(|row| {
            let name: Option<String> = row.try_get("name").map_err(db_err)?;
            let name = name.ok_or_else(|| {
                AppError::Internal(format!(
                    "companion store index {index_name} contains an expression instead of a column"
                ))
            })?;
            let descending = row.get::<i64, _>("descending") != 0;
            Ok((name, descending))
        })
        .collect()
}

async fn validate_table_contract(
    pool: &SqlitePool,
    contract: &TableContract,
    table_sql: &str,
) -> Result<(), AppError> {
    let columns = sqlx::query(
        "SELECT cid, name, type, \"notnull\" AS not_null, pk, hidden \
         FROM pragma_table_xinfo(?) ORDER BY cid",
    )
    .bind(contract.name)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    if columns.len() != contract.columns.len() {
        let actual: Vec<String> = columns.iter().map(|row| row.get("name")).collect();
        let expected: Vec<&str> = contract.columns.iter().map(|column| column.name).collect();
        return Err(AppError::Internal(format!(
            "companion store table {} column set is not the exact v3 baseline: expected {expected:?}, found {actual:?}",
            contract.name
        )));
    }
    for (actual, expected) in columns.iter().zip(contract.columns) {
        let name: String = actual.get("name");
        let declared_type: String = actual.get("type");
        let not_null = actual.get::<i64, _>("not_null") != 0;
        let primary_key_position = actual.get::<i64, _>("pk");
        let hidden = actual.get::<i64, _>("hidden");
        if name != expected.name
            || !declared_type.eq_ignore_ascii_case(expected.declared_type)
            || not_null != expected.not_null
            || primary_key_position != expected.primary_key_position
            || hidden != 0
        {
            return Err(AppError::Internal(format!(
                "companion store table {} column contract mismatch at {}: expected \
                 (name={}, type={}, not_null={}, pk={}), found \
                 (name={name}, type={declared_type}, not_null={not_null}, pk={primary_key_position}, hidden={hidden})",
                contract.name,
                expected.name,
                expected.name,
                expected.declared_type,
                expected.not_null,
                expected.primary_key_position,
            )));
        }
    }

    let normalized = normalized_schema_sql(table_sql);
    if !normalized.contains("idintegerprimarykeyautoincrement") {
        return Err(AppError::Internal(format!(
            "companion store table {} is not the v3 AUTOINCREMENT baseline",
            contract.name
        )));
    }
    for column in contract.uuidv7_columns {
        let required = [
            format!("length({column})=36"),
            format!("lower({column})={column}"),
            format!("{column}glob'????????-????-7???-[89ab]???-????????????'"),
            format!("replace({column},'-','')notglob'*[^0-9a-f]*'"),
        ];
        if let Some(missing) = required
            .iter()
            .find(|fragment| !normalized.contains(fragment.as_str()))
        {
            return Err(AppError::Internal(format!(
                "companion store table {} column {column} is missing UUIDv7 CHECK fragment {missing}",
                contract.name
            )));
        }
    }
    for fragment in contract.required_sql_fragments {
        if !normalized.contains(fragment) {
            return Err(AppError::Internal(format!(
                "companion store table {} is missing required CHECK fragment {fragment}",
                contract.name
            )));
        }
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct UniqueIndexShape {
        columns: Vec<String>,
        origin: String,
        partial: bool,
    }

    let index_rows = sqlx::query(
        "SELECT name, \"unique\" AS is_unique, origin, partial \
         FROM pragma_index_list(?)",
    )
    .bind(contract.name)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let mut actual_unique = Vec::new();
    for row in index_rows {
        if row.get::<i64, _>("is_unique") == 0 {
            continue;
        }
        let index_name: String = row.get("name");
        let columns = index_key_columns(pool, &index_name)
            .await?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        actual_unique.push(UniqueIndexShape {
            columns,
            origin: row.get("origin"),
            partial: row.get::<i64, _>("partial") != 0,
        });
    }
    actual_unique.sort();
    let mut expected_unique: Vec<UniqueIndexShape> = contract
        .unique_indexes
        .iter()
        .map(|index| UniqueIndexShape {
            columns: index.columns.iter().map(|column| (*column).to_owned()).collect(),
            origin: index.origin.to_owned(),
            partial: index.partial,
        })
        .collect();
    expected_unique.sort();
    if actual_unique != expected_unique {
        return Err(AppError::Internal(format!(
            "companion store table {} unique index contract mismatch: expected {expected_unique:?}, found {actual_unique:?}",
            contract.name
        )));
    }

    let foreign_keys = sqlx::query("SELECT * FROM pragma_foreign_key_list(?)")
        .bind(contract.name)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
    if !foreign_keys.is_empty() {
        return Err(AppError::Internal(format!(
            "companion store table {} contains physical foreign keys",
            contract.name
        )));
    }
    Ok(())
}

async fn validate_named_indexes(pool: &SqlitePool) -> Result<(), AppError> {
    let actual_names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master \
         WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let expected_names: std::collections::BTreeSet<&str> =
        BASELINE_INDEXES.iter().map(|index| index.name).collect();
    let actual_names_set: std::collections::BTreeSet<&str> =
        actual_names.iter().map(String::as_str).collect();
    if actual_names_set != expected_names {
        return Err(AppError::Internal(format!(
            "companion store index set is not the exact v3 baseline: expected {expected_names:?}, found {actual_names_set:?}"
        )));
    }

    for contract in BASELINE_INDEXES {
        let row = sqlx::query(
            "SELECT tbl_name, sql FROM sqlite_master WHERE type = 'index' AND name = ?",
        )
        .bind(contract.name)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
        let table: String = row.get("tbl_name");
        let sql: String = row.try_get("sql").map_err(db_err)?;
        let list_row = sqlx::query(
            "SELECT \"unique\" AS is_unique, origin, partial \
             FROM pragma_index_list(?) WHERE name = ?",
        )
        .bind(contract.table)
        .bind(contract.name)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| {
            AppError::Internal(format!(
                "companion store table {} is missing index {}",
                contract.table, contract.name
            ))
        })?;
        let unique = list_row.get::<i64, _>("is_unique") != 0;
        let origin: String = list_row.get("origin");
        let partial = list_row.get::<i64, _>("partial") != 0;
        let actual_columns = index_key_columns(pool, contract.name).await?;
        let expected_columns: Vec<(String, bool)> = contract
            .columns
            .iter()
            .map(|column| (column.name.to_owned(), column.descending))
            .collect();
        if table != contract.table
            || unique != contract.unique
            || partial != contract.partial
            || origin != "c"
            || actual_columns != expected_columns
        {
            return Err(AppError::Internal(format!(
                "companion store index {} does not match the v3 baseline: \
                 table={table}, unique={unique}, partial={partial}, origin={origin}, columns={actual_columns:?}",
                contract.name
            )));
        }
        if let Some(fragment) = contract.where_fragment
            && !normalized_schema_sql(&sql).contains(fragment)
        {
            return Err(AppError::Internal(format!(
                "companion store partial index {} is missing predicate {fragment}",
                contract.name
            )));
        }
    }
    Ok(())
}

async fn validate_baseline_schema(pool: &SqlitePool) -> Result<(), AppError> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    if version != STORE_VERSION {
        return Err(AppError::Internal(format!(
            "companion store contract version mismatch: expected {STORE_VERSION}, found {version}"
        )));
    }
    for table in BASELINE_TABLES {
        let sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table.name)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| AppError::Internal(format!("companion store missing table {}", table.name)))?;
        validate_table_contract(pool, table, &sql).await?;
    }
    validate_named_indexes(pool).await?;
    let trigger_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger'")
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    if trigger_count != 0 {
        return Err(AppError::Internal(
            "companion store v3 must not contain physical triggers".into(),
        ));
    }
    let user_tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let expected: std::collections::BTreeSet<&str> =
        BASELINE_TABLES.iter().map(|table| table.name).collect();
    let actual: std::collections::BTreeSet<&str> =
        user_tables.iter().map(String::as_str).collect();
    if actual != expected {
        return Err(AppError::Internal(format!(
            "companion store table set is not the exact v3 baseline: expected {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

async fn create_baseline_schema(pool: &SqlitePool) -> Result<(), AppError> {
    sqlx::raw_sql(SCHEMA).execute(pool).await.map_err(db_err)?;
    sqlx::raw_sql(&format!("PRAGMA user_version = {STORE_VERSION}"))
        .execute(pool)
        .await
        .map_err(db_err)?;
    validate_baseline_schema(pool).await
}

fn row_to_memory(row: &sqlx::sqlite::SqliteRow) -> Result<CompanionMemory, AppError> {
    let tags: String = row.get("tags");
    let memory_id: String = row.get("memory_id");
    CompanionMemoryId::try_from(memory_id.as_str())
        .map_err(|error| invalid_disk_id("memory id", &memory_id, error))?;
    let parsed_tags = serde_json::from_str(&tags).map_err(|error| {
        AppError::Internal(format!(
            "companion store memory '{}' contains invalid tags JSON: {error}",
            memory_id
        ))
    })?;
    let scope_kind: String = row.try_get("scope_kind").map_err(db_err)?;
    let scope_companion_id: Option<String> = row.try_get("scope_companion_id").map_err(db_err)?;
    match (scope_kind.as_str(), scope_companion_id.as_deref()) {
        ("user", None) => {}
        ("companion", Some(owner)) => {
            CompanionId::try_from(owner)
                .map_err(|error| invalid_disk_id("memory scope companion id", owner, error))?;
        }
        _ => {
            return Err(AppError::Internal(format!(
                "companion store contains invalid memory scope: kind={scope_kind:?}, owner={scope_companion_id:?}"
            )));
        }
    }
    Ok(CompanionMemory {
        memory_id,
        kind: row.get("kind"),
        content: row.get("content"),
        tags: parsed_tags,
        importance: row.get("importance"),
        strength: row.get("strength"),
        pinned: row.get::<i64, _>("pinned") != 0,
        source: row.get("source"),
        status: row.get("status"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_reinforced_at: row.get("last_reinforced_at"),
        scope_kind,
        scope_companion_id,
    })
}

/// Local-time day key (`YYYYMMDD`) for a ms-epoch timestamp — the partition key
/// for session-window digests. Uses the local timezone to stay consistent with
/// the event collector's `events/YYYYMMDD.jsonl` day boundaries.
pub fn local_day(ts_ms: TimestampMs) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|d| d.format("%Y%m%d").to_string())
        .unwrap_or_else(|| "00000000".into())
}

fn row_to_window(row: &sqlx::sqlite::SqliteRow) -> Result<SessionWindow, AppError> {
    let session_window_id: String = row.get("session_window_id");
    CompanionSessionWindowId::try_from(session_window_id.as_str())
        .map_err(|error| invalid_disk_id("session-window id", &session_window_id, error))?;
    let companion_id: String = row.get("companion_id");
    CompanionId::try_from(companion_id.as_str())
        .map_err(|error| invalid_disk_id("session-window companion id", &companion_id, error))?;
    let conversation_id: String = row.get("conversation_id");
    ConversationId::try_from(conversation_id.as_str())
        .map_err(|error| invalid_disk_id("session-window conversation id", &conversation_id, error))?;
    let highlights: Option<String> = row.try_get("highlights").map_err(db_err)?;
    if let Some(raw) = highlights.as_deref() {
        serde_json::from_str::<serde_json::Value>(raw).map_err(|error| {
            AppError::Internal(format!(
                "companion store session window '{}' contains invalid highlights JSON: {error}",
                session_window_id
            ))
        })?;
    }
    Ok(SessionWindow {
        session_window_id,
        companion_id,
        conversation_id,
        session_day: row.get("session_day"),
        started_at: row.get("started_at"),
        last_activity_at: row.get("last_activity_at"),
        closed_at: row.try_get("closed_at").map_err(db_err)?,
        status: row.get("status"),
        message_count: row.get("message_count"),
        boundary_ts: row.get("boundary_ts"),
        digest: row.try_get("digest").map_err(db_err)?,
        highlights,
        token_estimate: row.get("token_estimate"),
    })
}

fn row_to_companion_thread(row: &sqlx::sqlite::SqliteRow) -> Result<CompanionThread, AppError> {
    let conversation_id: String = row.get("conversation_id");
    ConversationId::try_from(conversation_id.as_str())
        .map_err(|error| invalid_disk_id("thread conversation id", &conversation_id, error))?;
    let companion_id: String = row.get("companion_id");
    CompanionId::try_from(companion_id.as_str())
        .map_err(|error| invalid_disk_id("thread companion id", &companion_id, error))?;
    Ok(CompanionThread {
        conversation_id,
        companion_id,
        title: row.get("title"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn row_to_learn_run(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<CompanionLearnRun, AppError> {
    let learn_run_id: String = row.get("learn_run_id");
    CompanionLearnRunId::try_from(learn_run_id.as_str())
        .map_err(|error| invalid_disk_id("learn-run id", &learn_run_id, error))?;
    Ok(CompanionLearnRun {
        learn_run_id,
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        status: row.get("status"),
        events_processed: row.get("events_processed"),
        memories_added: row.get("memories_added"),
        suggestions_added: row.get("suggestions_added"),
        error: row.get("error"),
        summary: row.get("summary"),
    })
}

fn row_to_suggestion(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<CompanionSuggestion, AppError> {
    let suggestion_id: String = row.get("suggestion_id");
    CompanionSuggestionId::try_from(suggestion_id.as_str())
        .map_err(|error| invalid_disk_id("suggestion id", &suggestion_id, error))?;
    let action: Option<String> = row.get("action");
    let kind: String = row.get("kind");
    let action = action
        .map(|raw| -> Result<serde_json::Value, AppError> {
            let action: serde_json::Value = serde_json::from_str(&raw).map_err(|error| {
                AppError::Internal(format!(
                    "companion store suggestion '{}' contains invalid action JSON: {error}",
                    suggestion_id
                ))
            })?;
            validate_suggestion_action(&kind, &action).map_err(|error| {
                AppError::Internal(format!(
                    "companion store suggestion '{}' contains invalid action: {error}",
                    suggestion_id
                ))
            })?;
            Ok(action)
        })
        .transpose()?;
    Ok(CompanionSuggestion {
        suggestion_id,
        kind,
        title: row.get("title"),
        body: row.get("body"),
        action,
        status: row.get("status"),
        created_at: row.get("created_at"),
        decided_at: row.get("decided_at"),
    })
}

fn validate_suggestion_action(
    kind: &str,
    action: &serde_json::Value,
) -> Result<(), AppError> {
    if kind != "create_skill" {
        return Ok(());
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CreateSkillAction {
        #[serde(rename = "type")]
        action_type: String,
        companion_id: String,
        companion_skill_id: String,
    }
    let action: CreateSkillAction = serde_json::from_value(action.clone())
        .map_err(|error| AppError::BadRequest(format!("invalid create_skill action: {error}")))?;
    if action.action_type != "create_skill" {
        return Err(AppError::BadRequest(
            "create_skill action type must be 'create_skill'".into(),
        ));
    }
    validate_companion_id(&action.companion_id, "create_skill action companion_id")?;
    validate_uuidv7(&action.companion_skill_id).map_err(|error| {
        AppError::BadRequest(format!(
            "invalid create_skill action companion_skill_id: {error}"
        ))
    })?;
    Ok(())
}

impl CompanionStore {
    /// Open (or create) the v3 baseline `{companion_dir}/memory.db`.
    pub async fn open(companion_dir: &Path) -> Result<Self, AppError> {
        std::fs::create_dir_all(companion_dir)
            .map_err(|e| AppError::Internal(format!("create companion dir: {e}")))?;
        let database_path = companion_dir.join("memory.db");
        let database_exists = match std::fs::symlink_metadata(&database_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(AppError::Internal(format!(
                    "companion store path is not a regular file: {}",
                    database_path.display()
                )));
            }
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "inspect companion store {}: {error}",
                    database_path.display()
                )));
            }
        };
        let opts = SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(!database_exists)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        {
            let bootstrap = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts.clone())
                .await
                .map_err(db_err)?;
            let init = if database_exists {
                validate_baseline_schema(&bootstrap).await
            } else {
                create_baseline_schema(&bootstrap).await
            };
            bootstrap.close().await;
            init?;
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(opts)
            .await
            .map_err(db_err)?;
        validate_baseline_schema(&pool).await?;
        let store = Self { pool };
        // Record the live store for the export/import routes (see LIVE_STORE).
        let _ = LIVE_STORE.set((companion_dir.to_path_buf(), store.clone()));
        Ok(store)
    }

    /// In-memory store for tests. The db lives inside the pool's single
    /// connection, so (unlike `open`) schema bootstrap must run on that same
    /// pool — a separate bootstrap connection would see a different db.
    pub async fn open_memory() -> Result<Self, AppError> {
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(db_err)?;
        create_baseline_schema(&pool).await?;
        Ok(Self { pool })
    }

    // ----- state kv -----

    pub async fn get_state(&self, key: &str) -> Result<Option<String>, AppError> {
        let row = sqlx::query("SELECT value FROM companion_state WHERE state_key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.map(|r| r.get("value")))
    }

    pub async fn set_state(&self, key: &str, value: &str) -> Result<(), AppError> {
        sqlx::query("INSERT INTO companion_state(state_key, value) VALUES(?, ?) ON CONFLICT(state_key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn get_state_i64(&self, key: &str) -> Result<i64, AppError> {
        match self.get_state(key).await? {
            None => Ok(0),
            Some(value) => value.parse().map_err(|error| {
                AppError::Internal(format!(
                    "companion state {key:?} contains invalid integer {value:?}: {error}"
                ))
            }),
        }
    }

    /// Atomic XP increment (single upsert — concurrent callers never lose a
    /// delta to read-modify-write interleaving). Returns the new total.
    pub async fn add_xp(&self, delta: i64) -> Result<i64, AppError> {
        let row = sqlx::query(
            "INSERT INTO companion_state(state_key, value) VALUES('xp', ?)
             ON CONFLICT(state_key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + ? AS TEXT)
             RETURNING CAST(value AS INTEGER) AS xp",
        )
        .bind(delta.to_string())
        .bind(delta)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.get("xp"))
    }

    // ----- per-companion state kv (companion_runtime_state) -----

    pub async fn get_companion_state(&self, companion_id: &str, key: &str) -> Result<Option<String>, AppError> {
        validate_companion_id(companion_id, "companion state companion_id")?;
        let row = sqlx::query("SELECT value FROM companion_runtime_state WHERE companion_id = ? AND state_key = ?")
            .bind(companion_id)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.map(|r| r.get("value")))
    }

    pub async fn set_companion_state(&self, companion_id: &str, key: &str, value: &str) -> Result<(), AppError> {
        validate_companion_id(companion_id, "companion state companion_id")?;
        sqlx::query(
            "INSERT INTO companion_runtime_state(companion_id, state_key, value) VALUES(?, ?, ?)
             ON CONFLICT(companion_id, state_key) DO UPDATE SET value = excluded.value",
        )
        .bind(companion_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn delete_companion_state(&self, companion_id: &str, key: &str) -> Result<(), AppError> {
        validate_companion_id(companion_id, "companion state companion_id")?;
        sqlx::query("DELETE FROM companion_runtime_state WHERE companion_id = ? AND state_key = ?")
            .bind(companion_id)
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn get_companion_state_i64(&self, companion_id: &str, key: &str) -> Result<i64, AppError> {
        match self.get_companion_state(companion_id, key).await? {
            None => Ok(0),
            Some(value) => value.parse().map_err(|error| {
                AppError::Internal(format!(
                    "companion state ({companion_id}, {key:?}) contains invalid integer {value:?}: {error}"
                ))
            }),
        }
    }

    /// Atomic per-companion XP increment (single upsert, key fixed to 'xp').
    /// Returns the companion's new total.
    pub async fn add_companion_xp(&self, companion_id: &str, delta: i64) -> Result<i64, AppError> {
        validate_companion_id(companion_id, "companion xp companion_id")?;
        let row = sqlx::query(
            "INSERT INTO companion_runtime_state(companion_id, state_key, value) VALUES(?, 'xp', ?)
             ON CONFLICT(companion_id, state_key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + ? AS TEXT)
             RETURNING CAST(value AS INTEGER) AS xp",
        )
        .bind(companion_id)
        .bind(delta.to_string())
        .bind(delta)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.get("xp"))
    }

    /// Grant the same XP delta to every listed companion (shared achievements like
    /// learn runs and accepted suggestions).
    pub async fn add_xp_all(&self, companion_ids: &[String], delta: i64) -> Result<(), AppError> {
        for companion_id in companion_ids {
            self.add_companion_xp(companion_id, delta).await?;
        }
        Ok(())
    }

    /// Remove every per-companion row owned by `companion_id` (runtime kv + companion
    /// thread registrations + private memories/skills/session windows) in one
    /// transaction. Used by companion deletion.
    pub async fn delete_companion_rows(&self, companion_id: &str) -> Result<(), AppError> {
        validate_companion_id(companion_id, "deleted companion_id")?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        for sql in [
            "DELETE FROM companion_memories WHERE scope_kind = 'companion' AND scope_companion_id = ?",
            "DELETE FROM companion_skills WHERE scope_kind = 'companion' AND scope_companion_id = ?",
            "DELETE FROM companion_session_windows WHERE companion_id = ?",
            "DELETE FROM companion_runtime_state WHERE companion_id = ?",
            "DELETE FROM companion_threads WHERE companion_id = ?",
        ] {
            sqlx::query(sql)
                .bind(companion_id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    /// Audit all logical companion references after the roster is loaded.
    /// Physical foreign keys are intentionally absent in v3, so startup must
    /// reject rows whose parent companion no longer exists instead of exposing
    /// partially orphaned side-store state.
    pub async fn validate_companion_references(
        &self,
        live_companion_ids: &std::collections::HashSet<String>,
    ) -> Result<(), AppError> {
        let references = [
            ("companion_memories", "scope_companion_id", "scope_kind = 'companion'"),
            ("companion_skills", "scope_companion_id", "scope_kind = 'companion'"),
            ("companion_runtime_state", "companion_id", "1 = 1"),
            ("companion_threads", "companion_id", "1 = 1"),
            ("companion_session_windows", "companion_id", "1 = 1"),
        ];
        for (table, column, predicate) in references {
            let sql = format!(
                "SELECT DISTINCT {column} FROM {table} WHERE {predicate} AND {column} IS NOT NULL"
            );
            let values: Vec<String> = sqlx::query_scalar(&sql)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
            for value in values {
                CompanionId::try_from(value.as_str())
                    .map_err(|error| invalid_disk_id("logical companion reference", &value, error))?;
                if !live_companion_ids.contains(&value) {
                    return Err(AppError::Internal(format!(
                        "companion store table {table} contains orphaned companion reference {value:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    // ----- memories -----

    pub async fn insert_memory(
        &self,
        kind: &str,
        content: &str,
        tags: &[String],
        importance: f64,
        source: &str,
    ) -> Result<CompanionMemory, AppError> {
        // Shared insert used by the learner hub and manual memory creation.
        self.insert_memory_scoped(kind, content, tags, importance, source, MemoryScope::Shared).await
    }

    /// Insert a memory with an explicit [`MemoryScope`]. Chat saves attribute to
    /// the owning companion (private); the learner and manual adds default shared.
    pub async fn insert_memory_scoped(
        &self,
        kind: &str,
        content: &str,
        tags: &[String],
        importance: f64,
        source: &str,
        scope: MemoryScope,
    ) -> Result<CompanionMemory, AppError> {
        // Best-effort redaction before any secret reaches durable storage.
        // Covers both write paths (manual save_memory and the distill learner),
        // which both funnel through here.
        let content = nomi_redact::redact_secrets(content);
        let now = now_ms();
        let (scope_kind, scope_companion_id) = scope.columns()?;
        let mem = CompanionMemory {
            memory_id: CompanionMemoryId::new().into_string(),
            kind: kind.to_owned(),
            content: content.into_owned(),
            tags: tags.to_vec(),
            importance: importance.clamp(0.0, 1.0),
            strength: importance.clamp(0.0, 1.0),
            pinned: false,
            source: source.to_owned(),
            status: "active".into(),
            created_at: now,
            updated_at: now,
            last_reinforced_at: now,
            scope_kind: scope_kind.to_owned(),
            scope_companion_id,
        };
        sqlx::query(
            "INSERT INTO companion_memories(memory_id, kind, content, tags, importance, strength, pinned, source, status, created_at, updated_at, last_reinforced_at, scope_kind, scope_companion_id)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&mem.memory_id)
        .bind(&mem.kind)
        .bind(&mem.content)
        .bind(
            serde_json::to_string(&mem.tags)
                .map_err(|error| AppError::Internal(format!("serialize memory tags: {error}")))?,
        )
        .bind(mem.importance)
        .bind(mem.strength)
        .bind(mem.pinned as i64)
        .bind(&mem.source)
        .bind(&mem.status)
        .bind(mem.created_at)
        .bind(mem.updated_at)
        .bind(mem.last_reinforced_at)
        .bind(&mem.scope_kind)
        .bind(&mem.scope_companion_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(mem)
    }

    /// Crude dedup guard: an active memory of the same kind whose normalized
    /// content equals the candidate, or contains it (either direction) when
    /// the two are close in length. The length-ratio guard stops a short
    /// memory ("主人用 Rust") from swallowing a longer, genuinely distinct
    /// one that merely embeds the same phrase.
    pub async fn find_similar_active(&self, kind: &str, content: &str) -> Result<Option<String>, AppError> {
        const CONTAINMENT_MIN_RATIO: f64 = 0.6;
        let norm = content.trim().to_lowercase();
        let rows = sqlx::query("SELECT memory_id, content FROM companion_memories WHERE kind = ? AND status = 'active'")
            .bind(kind)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        for row in rows {
            let existing: String = row.get("content");
            let existing_norm = existing.trim().to_lowercase();
            if existing_norm == norm {
                let id: String = row.get("memory_id");
                CompanionMemoryId::try_from(id.as_str())
                    .map_err(|error| invalid_disk_id("memory id", &id, error))?;
                return Ok(Some(id));
            }
            let (short_len, long_len) = {
                let a = norm.chars().count();
                let b = existing_norm.chars().count();
                (a.min(b), a.max(b))
            };
            let close_in_length = long_len > 0 && (short_len as f64 / long_len as f64) >= CONTAINMENT_MIN_RATIO;
            if close_in_length && (existing_norm.contains(&norm) || norm.contains(&existing_norm)) {
                let id: String = row.get("memory_id");
                CompanionMemoryId::try_from(id.as_str())
                    .map_err(|error| invalid_disk_id("memory id", &id, error))?;
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub async fn list_memories(&self, filter: &MemoryFilter) -> Result<Vec<CompanionMemory>, AppError> {
        if let Some(companion_id) = filter.scope_companion_id.as_deref() {
            validate_companion_id(companion_id, "memory filter companion_id")?;
        }
        let mut sql = format!("SELECT * FROM companion_memories{}", memory_filter_clause(filter));
        sql.push_str(" ORDER BY pinned DESC, strength DESC, updated_at DESC LIMIT ? OFFSET ?");
        let mut query = sqlx::query(&sql);
        if let Some(kind) = &filter.kind {
            query = query.bind(kind);
        }
        if let Some(q) = &filter.q {
            query = query.bind(format!("%{q}%"));
        }
        if let Some(status) = &filter.status {
            query = query.bind(status);
        }
        if let Some(cid) = &filter.scope_companion_id {
            query = query.bind(cid);
        }
        let limit = if filter.limit <= 0 { 100 } else { filter.limit.min(500) };
        query = query.bind(limit).bind(filter.offset.max(0));
        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    pub async fn list_memory_page(&self, filter: &MemoryFilter) -> Result<MemoryPage, AppError> {
        if let Some(companion_id) = filter.scope_companion_id.as_deref() {
            validate_companion_id(companion_id, "memory filter companion_id")?;
        }
        let mut items_sql = format!("SELECT * FROM companion_memories{}", memory_filter_clause(filter));
        items_sql.push_str(" ORDER BY pinned DESC, strength DESC, updated_at DESC LIMIT ? OFFSET ?");
        let mut items_query = sqlx::query(&items_sql);
        if let Some(kind) = &filter.kind {
            items_query = items_query.bind(kind);
        }
        if let Some(q) = &filter.q {
            items_query = items_query.bind(format!("%{q}%"));
        }
        if let Some(status) = &filter.status {
            items_query = items_query.bind(status);
        }
        if let Some(cid) = &filter.scope_companion_id {
            items_query = items_query.bind(cid);
        }
        let limit = if filter.limit <= 0 { 100 } else { filter.limit.min(500) };
        let rows = items_query
            .bind(limit)
            .bind(filter.offset.max(0))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        let count_sql = format!("SELECT COUNT(*) AS n FROM companion_memories{}", memory_filter_clause(filter));
        let mut count_query = sqlx::query(&count_sql);
        if let Some(kind) = &filter.kind {
            count_query = count_query.bind(kind);
        }
        if let Some(q) = &filter.q {
            count_query = count_query.bind(format!("%{q}%"));
        }
        if let Some(status) = &filter.status {
            count_query = count_query.bind(status);
        }
        if let Some(cid) = &filter.scope_companion_id {
            count_query = count_query.bind(cid);
        }
        let total = count_query.fetch_one(&self.pool).await.map_err(db_err)?.get("n");

        Ok(MemoryPage {
            items: rows.iter().map(row_to_memory).collect::<Result<Vec<_>, _>>()?,
            total,
        })
    }

    pub async fn count_memories(&self, status: &str) -> Result<i64, AppError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM companion_memories WHERE status = ?")
            .bind(status)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.get("n"))
    }

    pub async fn update_memory(
        &self,
        memory_id: &str,
        content: Option<&str>,
        pinned: Option<bool>,
        status: Option<&str>,
        scope: Option<MemoryScope>,
    ) -> Result<(), AppError> {
        CompanionMemoryId::try_from(memory_id)
            .map_err(|error| AppError::BadRequest(format!("invalid memory id: {error}")))?;
        // Validate + redact edited content symmetrically with insert_memory_scoped:
        // a user/agent edit must not bypass the empty-content guard or secret
        // redaction that the insert path enforces.
        let redacted: Option<String> = match content {
            Some(c) => {
                let trimmed = c.trim();
                if trimmed.is_empty() {
                    return Err(AppError::BadRequest("memory content is empty".into()));
                }
                Some(nomi_redact::redact_secrets(trimmed).into_owned())
            }
            None => None,
        };
        let scope_changed = scope.is_some();
        let scope_columns = scope.as_ref().map(MemoryScope::columns).transpose()?;
        let scope_kind = scope_columns.as_ref().map(|(kind, _)| *kind);
        let scope_companion_id = scope_columns
            .as_ref()
            .and_then(|(_, companion_id)| companion_id.as_deref());
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE companion_memories SET
               content = COALESCE(?, content),
               pinned = COALESCE(?, pinned),
               status = COALESCE(?, status),
               scope_kind = CASE WHEN ? THEN ? ELSE scope_kind END,
               scope_companion_id = CASE WHEN ? THEN ? ELSE scope_companion_id END,
               updated_at = ?
             WHERE memory_id = ?",
        )
        .bind(redacted.as_deref())
        .bind(pinned.map(|p| p as i64))
        .bind(status)
        .bind(scope_changed)
        .bind(scope_kind)
        .bind(scope_changed)
        .bind(scope_companion_id)
        .bind(now)
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!("memory '{memory_id}' not found")));
        }
        Ok(())
    }

    pub async fn delete_memory(&self, memory_id: &str) -> Result<(), AppError> {
        CompanionMemoryId::try_from(memory_id)
            .map_err(|error| AppError::BadRequest(format!("invalid memory id: {error}")))?;
        sqlx::query("DELETE FROM companion_memories WHERE memory_id = ?")
            .bind(memory_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Reinforce: bump strength toward 1.0 and refresh the reinforcement clock.
    pub async fn reinforce_memories(&self, ids: &[String]) -> Result<(), AppError> {
        let now = now_ms();
        for id in ids {
            CompanionMemoryId::try_from(id.as_str())
                .map_err(|error| AppError::BadRequest(format!("invalid memory id: {error}")))?;
            sqlx::query(
                "UPDATE companion_memories SET strength = MIN(1.0, strength + 0.2), last_reinforced_at = ?, updated_at = ?, status = 'active' WHERE memory_id = ?",
            )
            .bind(now)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    /// Supersede: archive replaced memories (kept for provenance).
    pub async fn archive_memories(&self, ids: &[String]) -> Result<(), AppError> {
        let now = now_ms();
        for id in ids {
            CompanionMemoryId::try_from(id.as_str())
                .map_err(|error| AppError::BadRequest(format!("invalid memory id: {error}")))?;
            sqlx::query("UPDATE companion_memories SET status = 'archived', updated_at = ? WHERE memory_id = ?")
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
        }
        Ok(())
    }

    /// Apply exponential decay to every non-pinned active memory, archiving
    /// the ones that fall below the threshold. Returns archived count.
    pub async fn decay_memories(&self) -> Result<i64, AppError> {
        let now = now_ms();
        let rows = sqlx::query(
            "SELECT memory_id, kind, strength, last_reinforced_at FROM companion_memories WHERE status = 'active' AND pinned = 0",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut archived = 0i64;
        for row in rows {
            let kind: String = row.get("kind");
            let Some(half_life) = half_life_days(&kind) else { continue };
            let strength: f64 = row.get("strength");
            let last: i64 = row.get("last_reinforced_at");
            let age_days = ((now - last).max(0)) as f64 / 86_400_000.0;
            let decayed = strength * 0.5f64.powf(age_days / half_life);
            let id: String = row.get("memory_id");
            CompanionMemoryId::try_from(id.as_str())
                .map_err(|error| invalid_disk_id("memory id", &id, error))?;
            if decayed < ARCHIVE_THRESHOLD {
                sqlx::query("UPDATE companion_memories SET strength = ?, status = 'archived', updated_at = ? WHERE memory_id = ?")
                    .bind(decayed)
                    .bind(now)
                    .bind(&id)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
                archived += 1;
            } else {
                sqlx::query("UPDATE companion_memories SET strength = ? WHERE memory_id = ?")
                    .bind(decayed)
                    .bind(&id)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
            }
        }
        Ok(archived)
    }

    /// Top memories for prompt injection: all pinned + per-kind top-N by
    /// strength, within a rough char budget. Scoped to `companion_id`: shared
    /// memories plus that companion's own private ones (others' private are
    /// never injected into this companion's prompt).
    pub async fn memories_for_injection(&self, companion_id: &str, per_kind: i64, char_budget: usize) -> Result<Vec<CompanionMemory>, AppError> {
        validate_companion_id(companion_id, "memory injection companion_id")?;
        let mut picked: Vec<CompanionMemory> = Vec::new();
        let pinned = sqlx::query(
            "SELECT * FROM companion_memories WHERE status = 'active' AND pinned = 1 AND (scope_kind = 'user' OR scope_companion_id = ?) ORDER BY strength DESC",
        )
        .bind(companion_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        picked.extend(pinned.iter().map(row_to_memory).collect::<Result<Vec<_>, _>>()?);
        for kind in MEMORY_KINDS {
            let rows = sqlx::query(
                "SELECT * FROM companion_memories WHERE status = 'active' AND pinned = 0 AND kind = ? AND (scope_kind = 'user' OR scope_companion_id = ?) ORDER BY strength DESC LIMIT ?",
            )
            .bind(kind)
            .bind(companion_id)
            .bind(per_kind)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            picked.extend(rows.iter().map(row_to_memory).collect::<Result<Vec<_>, _>>()?);
        }
        let mut used = 0usize;
        picked.retain(|m| {
            used += m.content.len();
            used <= char_budget
        });
        Ok(picked)
    }

    // ----- session windows (伙伴会话窗口归档) -----

    /// The companion's currently-open window, if any.
    pub async fn open_window(&self, companion_id: &str) -> Result<Option<SessionWindow>, AppError> {
        validate_companion_id(companion_id, "session-window companion_id")?;
        let row = sqlx::query(
            "SELECT * FROM companion_session_windows WHERE companion_id = ? AND status = 'open' \
             ORDER BY started_at DESC LIMIT 1",
        )
        .bind(companion_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.as_ref().map(row_to_window).transpose()
    }

    /// Get-or-create the companion's open window. A fresh window's `boundary_ts`
    /// is `now` unless `boundary_ts` overrides it (used when rolling over from a
    /// just-closed window so the new window excludes already-archived messages).
    pub async fn ensure_open_window(
        &self,
        companion_id: &str,
        conversation_id: &str,
        boundary_ts: TimestampMs,
    ) -> Result<SessionWindow, AppError> {
        validate_companion_id(companion_id, "session-window companion_id")?;
        validate_conversation_id(conversation_id, "session-window conversation_id")?;
        if let Some(w) = self.open_window(companion_id).await? {
            return Ok(w);
        }
        let now = now_ms();
        let w = SessionWindow {
            session_window_id: CompanionSessionWindowId::new().into_string(),
            companion_id: companion_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            session_day: local_day(now),
            started_at: now,
            last_activity_at: now,
            closed_at: None,
            status: "open".into(),
            message_count: 0,
            boundary_ts,
            digest: None,
            highlights: None,
            token_estimate: 0,
        };
        sqlx::query(
            "INSERT INTO companion_session_windows \
             (session_window_id, companion_id, conversation_id, session_day, started_at, last_activity_at, \
              closed_at, status, message_count, boundary_ts, digest, highlights, token_estimate) \
             VALUES(?,?,?,?,?,?,NULL,'open',0,?,NULL,NULL,0)",
        )
        .bind(&w.session_window_id)
        .bind(&w.companion_id)
        .bind(&w.conversation_id)
        .bind(&w.session_day)
        .bind(w.started_at)
        .bind(w.last_activity_at)
        .bind(w.boundary_ts)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(w)
    }

    /// Record activity on an open window (bumps `last_activity_at` and, when
    /// larger, `message_count`). Never regresses the count so a partial re-scan
    /// can't shrink it.
    pub async fn touch_window(&self, window_id: &str, last_activity_at: TimestampMs, message_count: i64) -> Result<(), AppError> {
        CompanionSessionWindowId::try_from(window_id)
            .map_err(|error| AppError::BadRequest(format!("invalid session-window id: {error}")))?;
        sqlx::query(
            "UPDATE companion_session_windows SET last_activity_at = ?, message_count = MAX(message_count, ?) \
             WHERE session_window_id = ? AND status = 'open'",
        )
        .bind(last_activity_at)
        .bind(message_count)
        .bind(window_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Close a window with its compressed digest. `status` is `archived` (has a
    /// digest) or `skipped` (too little content — digest stays NULL).
    pub async fn close_window(
        &self,
        window_id: &str,
        status: &str,
        digest: Option<&str>,
        highlights: Option<&str>,
        token_estimate: i64,
    ) -> Result<(), AppError> {
        CompanionSessionWindowId::try_from(window_id)
            .map_err(|error| AppError::BadRequest(format!("invalid session-window id: {error}")))?;
        if let Some(highlights) = highlights {
            serde_json::from_str::<serde_json::Value>(highlights).map_err(|error| {
                AppError::BadRequest(format!("invalid session-window highlights JSON: {error}"))
            })?;
        }
        sqlx::query(
            "UPDATE companion_session_windows \
             SET status = ?, digest = ?, highlights = ?, token_estimate = ?, closed_at = ? \
             WHERE session_window_id = ?",
        )
        .bind(status)
        .bind(digest)
        .bind(highlights)
        .bind(token_estimate)
        .bind(now_ms())
        .bind(window_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Archived digests for one companion, most-recent day first. `limit` caps rows.
    pub async fn list_digests(&self, companion_id: &str, limit: i64) -> Result<Vec<SessionWindow>, AppError> {
        validate_companion_id(companion_id, "session-window companion_id")?;
        let rows = sqlx::query(
            "SELECT * FROM companion_session_windows WHERE companion_id = ? AND status = 'archived' \
             ORDER BY started_at DESC LIMIT ?",
        )
        .bind(companion_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_window).collect()
    }

    /// Digests whose LOCAL start day falls in `[since_day, until_day]` (inclusive,
    /// `YYYYMMDD` string compare). Either bound may be empty to leave it open.
    pub async fn digests_in_range(&self, companion_id: &str, since_day: &str, until_day: &str) -> Result<Vec<SessionWindow>, AppError> {
        validate_companion_id(companion_id, "session-window companion_id")?;
        let rows = sqlx::query(
            "SELECT * FROM companion_session_windows \
             WHERE companion_id = ? AND status = 'archived' \
               AND (? = '' OR session_day >= ?) AND (? = '' OR session_day <= ?) \
             ORDER BY session_day ASC, started_at ASC",
        )
        .bind(companion_id)
        .bind(since_day)
        .bind(since_day)
        .bind(until_day)
        .bind(until_day)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_window).collect()
    }

    /// "去年今日" — archived digests whose day-of-year (`MMDD`) matches `mmdd`,
    /// excluding the current `session_day`, most-recent year first. `mmdd` is the
    /// 4-char suffix of a `YYYYMMDD` day.
    pub async fn digests_on_day_of_year(&self, companion_id: &str, mmdd: &str, exclude_day: &str, limit: i64) -> Result<Vec<SessionWindow>, AppError> {
        validate_companion_id(companion_id, "session-window companion_id")?;
        let rows = sqlx::query(
            "SELECT * FROM companion_session_windows \
             WHERE companion_id = ? AND status = 'archived' \
               AND substr(session_day, 5) = ? AND session_day != ? \
             ORDER BY session_day DESC LIMIT ?",
        )
        .bind(companion_id)
        .bind(mmdd)
        .bind(exclude_day)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_window).collect()
    }

    // ----- suggestions -----

    pub async fn insert_suggestion(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        action: Option<&serde_json::Value>,
    ) -> Result<CompanionSuggestion, AppError> {
        if let Some(action) = action {
            validate_suggestion_action(kind, action)?;
        }
        let now = now_ms();
        let s = CompanionSuggestion {
            suggestion_id: CompanionSuggestionId::new().into_string(),
            kind: kind.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
            action: action.cloned(),
            status: "new".into(),
            created_at: now,
            decided_at: None,
        };
        sqlx::query("INSERT INTO companion_suggestions(suggestion_id, kind, title, body, action, status, created_at) VALUES(?,?,?,?,?,?,?)")
            .bind(&s.suggestion_id)
            .bind(&s.kind)
            .bind(&s.title)
            .bind(&s.body)
            .bind(s.action.as_ref().map(|a| a.to_string()))
            .bind(&s.status)
            .bind(s.created_at)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(s)
    }

    /// Crude dedup guard for suggestions, mirroring [`find_similar_active`]:
    /// a pending (status='new') suggestion of the same kind whose normalized
    /// title equals the candidate's — or contains it (either direction) when
    /// the two are close in length — or whose normalized body equals the
    /// candidate's. Decided suggestions never block a fresh one: the owner
    /// may legitimately want a dismissed idea re-raised later.
    pub async fn find_similar_suggestion(&self, kind: &str, title: &str, body: &str) -> Result<Option<String>, AppError> {
        const CONTAINMENT_MIN_RATIO: f64 = 0.6;
        let norm_title = title.trim().to_lowercase();
        let norm_body = body.trim().to_lowercase();
        let rows = sqlx::query("SELECT suggestion_id, title, body FROM companion_suggestions WHERE kind = ? AND status = 'new'")
            .bind(kind)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        for row in rows {
            let existing_title: String = row.get("title");
            let existing_title = existing_title.trim().to_lowercase();
            if !norm_title.is_empty() && existing_title == norm_title {
                let id: String = row.get("suggestion_id");
                CompanionSuggestionId::try_from(id.as_str())
                    .map_err(|error| invalid_disk_id("suggestion id", &id, error))?;
                return Ok(Some(id));
            }
            let (short_len, long_len) = {
                let a = norm_title.chars().count();
                let b = existing_title.chars().count();
                (a.min(b), a.max(b))
            };
            let close_in_length = long_len > 0 && (short_len as f64 / long_len as f64) >= CONTAINMENT_MIN_RATIO;
            if close_in_length
                && !norm_title.is_empty()
                && (existing_title.contains(&norm_title) || norm_title.contains(&existing_title))
            {
                let id: String = row.get("suggestion_id");
                CompanionSuggestionId::try_from(id.as_str())
                    .map_err(|error| invalid_disk_id("suggestion id", &id, error))?;
                return Ok(Some(id));
            }
            if !norm_body.is_empty() {
                let existing_body: String = row.get("body");
                if existing_body.trim().to_lowercase() == norm_body {
                    let id: String = row.get("suggestion_id");
                    CompanionSuggestionId::try_from(id.as_str())
                        .map_err(|error| invalid_disk_id("suggestion id", &id, error))?;
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    /// "Touch" a still-pending suggestion the learner just re-derived: bump
    /// `created_at` so it re-floats to the top of the (created_at DESC)
    /// list as freshly reinforced evidence. The table has no updated_at or
    /// hit-count column — re-stamping the only timestamp is the minimal
    /// signal that the suggestion keeps coming up. Decided suggestions are
    /// never touched (their lifecycle is over).
    pub async fn touch_suggestion(&self, suggestion_id: &str) -> Result<(), AppError> {
        CompanionSuggestionId::try_from(suggestion_id)
            .map_err(|error| AppError::BadRequest(format!("invalid suggestion id: {error}")))?;
        sqlx::query("UPDATE companion_suggestions SET created_at = ? WHERE suggestion_id = ? AND status = 'new'")
            .bind(now_ms())
            .bind(suggestion_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn list_suggestions(&self, status: Option<&str>, limit: i64) -> Result<Vec<CompanionSuggestion>, AppError> {
        let rows = if let Some(status) = status {
            sqlx::query("SELECT * FROM companion_suggestions WHERE status = ? ORDER BY created_at DESC LIMIT ?")
                .bind(status)
                .bind(limit.clamp(1, 500))
                .fetch_all(&self.pool)
                .await
        } else {
            sqlx::query("SELECT * FROM companion_suggestions ORDER BY created_at DESC LIMIT ?")
                .bind(limit.clamp(1, 500))
                .fetch_all(&self.pool)
                .await
        }
        .map_err(db_err)?;
        rows.iter().map(row_to_suggestion).collect()
    }

    pub async fn list_suggestion_page(
        &self,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<SuggestionPage, AppError> {
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);
        let (rows, total) = if let Some(status) = status {
            let rows = sqlx::query(
                "SELECT * FROM companion_suggestions WHERE status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
            )
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM companion_suggestions WHERE status = ?")
                .bind(status)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?
                .get("n");
            (rows, total)
        } else {
            let rows = sqlx::query("SELECT * FROM companion_suggestions ORDER BY created_at DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
            let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM companion_suggestions")
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?
                .get("n");
            (rows, total)
        };

        Ok(SuggestionPage {
            items: rows.iter().map(row_to_suggestion).collect::<Result<Vec<_>, _>>()?,
            total,
        })
    }

    pub async fn count_suggestions(&self, status: &str) -> Result<i64, AppError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM companion_suggestions WHERE status = ?")
            .bind(status)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.get("n"))
    }

    /// Decide a suggestion. **Idempotent**: deciding an already-decided
    /// suggestion is a no-op that returns its current state (first decision
    /// wins) rather than an error — two surfaces (panel + desktop bubble) and
    /// double-clicks would otherwise race the `status = 'new'` guard and 404.
    /// Only a genuinely missing row is `NotFound`. The returned bool is
    /// `newly_decided`: true only when THIS call performed the new->decided
    /// transition, so callers can gate side effects (xp award, events) on it.
    pub async fn decide_suggestion(&self, suggestion_id: &str, accept: bool) -> Result<(CompanionSuggestion, bool), AppError> {
        CompanionSuggestionId::try_from(suggestion_id)
            .map_err(|error| AppError::BadRequest(format!("invalid suggestion id: {error}")))?;
        let status = if accept { "accepted" } else { "dismissed" };
        let result = sqlx::query("UPDATE companion_suggestions SET status = ?, decided_at = ? WHERE suggestion_id = ? AND status = 'new'")
            .bind(status)
            .bind(now_ms())
            .bind(suggestion_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        let newly_decided = result.rows_affected() >= 1;
        // Always read back: rows_affected == 0 means either the row is gone
        // (true 404) or it was already decided (idempotent success).
        let row = sqlx::query("SELECT * FROM companion_suggestions WHERE suggestion_id = ?")
            .bind(suggestion_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        match row {
            Some(row) => Ok((row_to_suggestion(&row)?, newly_decided)),
            None => Err(AppError::NotFound(format!(
                "suggestion '{suggestion_id}' not found"
            ))),
        }
    }

    // ----- learn runs -----

    pub async fn insert_learn_run(&self, run: &CompanionLearnRun) -> Result<(), AppError> {
        CompanionLearnRunId::try_from(run.learn_run_id.as_str())
            .map_err(|error| AppError::BadRequest(format!("invalid companion learn-run id: {error}")))?;
        sqlx::query(
            "INSERT INTO companion_learn_runs(learn_run_id, started_at, finished_at, status, events_processed, memories_added, suggestions_added, error, summary)
             VALUES(?,?,?,?,?,?,?,?,?)",
        )
        .bind(&run.learn_run_id)
        .bind(run.started_at)
        .bind(run.finished_at)
        .bind(&run.status)
        .bind(run.events_processed)
        .bind(run.memories_added)
        .bind(run.suggestions_added)
        .bind(&run.error)
        .bind(&run.summary)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn list_learn_runs(&self, limit: i64) -> Result<Vec<CompanionLearnRun>, AppError> {
        let rows = sqlx::query("SELECT * FROM companion_learn_runs ORDER BY started_at DESC LIMIT ?")
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_to_learn_run).collect()
    }

    // ----- export/import support (spec §4.8) -----

    /// Page size for the full-table dump cursors below.
    const DUMP_PAGE: i64 = 500;

    /// Every `companion_memories` row (all statuses, archived included), streamed
    /// out via an id cursor so an arbitrarily large table never needs one
    /// giant query. Ordered by id (stable across calls).
    pub async fn dump_memories_all(&self) -> Result<Vec<CompanionMemory>, AppError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let rows = sqlx::query("SELECT * FROM companion_memories WHERE memory_id > ? ORDER BY memory_id LIMIT ?")
                .bind(&cursor)
                .bind(Self::DUMP_PAGE)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
            let Some(last) = rows.last() else { break };
            let next_cursor: String = last.get("memory_id");
            CompanionMemoryId::try_from(next_cursor.as_str())
                .map_err(|error| invalid_disk_id("memory id", &next_cursor, error))?;
            cursor = next_cursor;
            out.extend(rows.iter().map(row_to_memory).collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    /// Every `companion_learn_runs` row via the same id cursor as
    /// [`dump_memories_all`]. Ordered by id.
    pub async fn dump_learn_runs_all(&self) -> Result<Vec<CompanionLearnRun>, AppError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let rows = sqlx::query("SELECT * FROM companion_learn_runs WHERE learn_run_id > ? ORDER BY learn_run_id LIMIT ?")
                .bind(&cursor)
                .bind(Self::DUMP_PAGE)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
            let Some(last) = rows.last() else { break };
            let next_cursor: String = last.get("learn_run_id");
            CompanionLearnRunId::try_from(next_cursor.as_str())
                .map_err(|error| invalid_disk_id("learn-run id", &next_cursor, error))?;
            cursor = next_cursor;
            out.extend(rows.iter().map(row_to_learn_run).collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    pub async fn get_memory(&self, memory_id: &str) -> Result<Option<CompanionMemory>, AppError> {
        CompanionMemoryId::try_from(memory_id)
            .map_err(|error| AppError::BadRequest(format!("invalid memory id: {error}")))?;
        let row = sqlx::query("SELECT * FROM companion_memories WHERE memory_id = ?")
            .bind(memory_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.as_ref().map(row_to_memory).transpose()
    }

    /// Fidelity insert for import: every field (memory_id, timestamps, strength,
    /// pinned, source, status, …) is written exactly as given — unlike
    /// [`insert_memory`], nothing is regenerated or clamped. The caller is
    /// responsible for id-collision handling (see `export::import_bundle`).
    pub async fn insert_memory_raw(&self, mem: &CompanionMemory) -> Result<(), AppError> {
        CompanionMemoryId::try_from(mem.memory_id.as_str())
            .map_err(|error| AppError::BadRequest(format!("invalid imported memory id: {error}")))?;
        match (mem.scope_kind.as_str(), mem.scope_companion_id.as_deref()) {
            ("user", None) => {}
            ("companion", Some(owner)) => validate_companion_id(owner, "imported memory scope companion_id")?,
            _ => {
                return Err(AppError::BadRequest(
                    "imported memory scope must be shared (user/None) or private (companion/Some(canonical ID))".into(),
                ));
            }
        }
        sqlx::query(
            "INSERT INTO companion_memories(memory_id, kind, content, tags, importance, strength, pinned, source, status, created_at, updated_at, last_reinforced_at, scope_kind, scope_companion_id)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&mem.memory_id)
        .bind(&mem.kind)
        .bind(&mem.content)
        .bind(
            serde_json::to_string(&mem.tags).map_err(|error| {
                AppError::BadRequest(format!("invalid imported memory tags: {error}"))
            })?,
        )
        .bind(mem.importance)
        .bind(mem.strength)
        .bind(mem.pinned as i64)
        .bind(&mem.source)
        .bind(&mem.status)
        .bind(mem.created_at)
        .bind(mem.updated_at)
        .bind(mem.last_reinforced_at)
        .bind(&mem.scope_kind)
        .bind(&mem.scope_companion_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn learn_run_exists(&self, id: &str) -> Result<bool, AppError> {
        CompanionLearnRunId::try_from(id)
            .map_err(|error| AppError::BadRequest(format!("invalid learn-run id: {error}")))?;
        let row = sqlx::query("SELECT 1 AS x FROM companion_learn_runs WHERE learn_run_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    // ----- companion threads -----

    /// Register a conversation as a companion thread (idempotent upsert).
    /// Both IDs must be canonical. Re-registering an existing thread refreshes
    /// title/clock and preserves the one-thread-per-companion invariant.
    pub async fn insert_companion_thread(
        &self,
        conversation_id: &str,
        companion_id: &str,
        title: &str,
    ) -> Result<CompanionThread, AppError> {
        validate_conversation_id(conversation_id, "companion thread conversation_id")?;
        validate_companion_id(companion_id, "companion thread companion_id")?;
        let now = now_ms();
        // The canonical conversation ID is the stable thread identity. An
        // upsert refreshes mutable thread metadata for that same entity.
        let row = sqlx::query(
            "INSERT INTO companion_threads(conversation_id, companion_id, title, created_at, updated_at) VALUES(?,?,?,?,?)
             ON CONFLICT(conversation_id) DO UPDATE SET companion_id = excluded.companion_id, title = excluded.title, updated_at = excluded.updated_at
             RETURNING conversation_id, companion_id, title, created_at, updated_at",
        )
        .bind(conversation_id)
        .bind(companion_id)
        .bind(title)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        row_to_companion_thread(&row)
    }

    /// Threads, most recently touched first — all of them, or only one companion's.
    pub async fn list_companion_threads(&self, companion_id: Option<&str>) -> Result<Vec<CompanionThread>, AppError> {
        if let Some(companion_id) = companion_id {
            validate_companion_id(companion_id, "companion thread companion_id")?;
        }
        let rows = if let Some(companion_id) = companion_id {
            sqlx::query("SELECT * FROM companion_threads WHERE companion_id = ? ORDER BY updated_at DESC")
                .bind(companion_id)
                .fetch_all(&self.pool)
                .await
        } else {
            sqlx::query("SELECT * FROM companion_threads ORDER BY updated_at DESC")
                .fetch_all(&self.pool)
                .await
        }
        .map_err(db_err)?;
        rows.iter().map(row_to_companion_thread).collect()
    }

    /// The owning companion of a registered thread. Only an unregistered
    /// conversation returns `None`; ownerless disk rows cannot be created by the v3 schema.
    pub async fn thread_companion_id(&self, conversation_id: &str) -> Result<Option<String>, AppError> {
        validate_conversation_id(conversation_id, "companion thread conversation_id")?;
        let row = sqlx::query("SELECT companion_id FROM companion_threads WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        let Some(row) = row else { return Ok(None) };
        let companion_id: String = row.get("companion_id");
        CompanionId::try_from(companion_id.as_str())
            .map_err(|error| invalid_disk_id("thread companion id", &companion_id, error))?;
        Ok(Some(companion_id))
    }

    pub async fn is_companion_thread(&self, conversation_id: &str) -> Result<bool, AppError> {
        validate_conversation_id(conversation_id, "companion thread conversation_id")?;
        let row = sqlx::query("SELECT 1 AS x FROM companion_threads WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    /// Rename and/or bump the activity clock of a thread.
    pub async fn touch_companion_thread(&self, conversation_id: &str, title: Option<&str>) -> Result<(), AppError> {
        validate_conversation_id(conversation_id, "companion thread conversation_id")?;
        let result = sqlx::query(
            "UPDATE companion_threads SET title = COALESCE(?, title), updated_at = ? WHERE conversation_id = ?",
        )
        .bind(title)
        .bind(now_ms())
        .bind(conversation_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "companion thread '{conversation_id}' not found"
            )));
        }
        Ok(())
    }

    pub async fn delete_companion_thread(&self, conversation_id: &str) -> Result<(), AppError> {
        validate_conversation_id(conversation_id, "companion thread conversation_id")?;
        sqlx::query("DELETE FROM companion_threads WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}


// ---------------------------------------------------------------------------
// 自进化：技能注册表 / 挖矿统计 / 反馈回流
// 正文以磁盘 SKILL.md 为事实源（见 nomifun-extension::skill_service）；这里只存
// 元数据 + 溯源 + 生命周期。scope_companion_id = NULL 表示 shared（全员可用）。
// ---------------------------------------------------------------------------

/// 一个伙伴自进化技能的注册表行。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionSkill {
    #[serde(deserialize_with = "deserialize_uuidv7_string")]
    pub companion_skill_id: String,
    pub scope_kind: String,
    pub skill_name: String,
    /// `None` = shared（全员可用）；`Some` is the canonical owning companion ID.
    pub scope_companion_id: Option<String>,
    pub status: String,
    pub source: String,
    pub confidence: f64,
    #[serde(deserialize_with = "deserialize_uuidv7_strings")]
    pub provenance_event_ids: Vec<String>,
    pub strength: f64,
    pub version: i64,
    /// Logical reference to the mined pattern that produced this skill.
    #[serde(deserialize_with = "deserialize_optional_uuidv7_string")]
    pub skill_pattern_id: Option<String>,
    pub usage_count: i64,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Originating mined-pattern signature ("" for manual/demonstrated skills);
    /// used to suppress a rejected pattern from re-proposal (纠偏回流).
    #[serde(default)]
    pub signature: String,
}

/// One page of skills visible to a companion and the number of matching rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompanionSkillPage {
    pub items: Vec<CompanionSkill>,
    pub total: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPattern {
    pub skill_pattern_id: String,
    pub signature: String,
    pub status: String,
}

fn row_to_skill(row: &sqlx::sqlite::SqliteRow) -> Result<CompanionSkill, AppError> {
    let companion_skill_id: String = row.get("companion_skill_id");
    validate_uuidv7(&companion_skill_id)
        .map_err(|error| invalid_disk_id("companion skill id", &companion_skill_id, error))?;
    let provenance: String = row.get("provenance_event_ids");
    let provenance_event_ids: Vec<String> = serde_json::from_str(&provenance).map_err(|error| {
        AppError::Internal(format!(
            "companion store skill '{}' contains invalid provenance_event_ids JSON: {error}",
            companion_skill_id
        ))
    })?;
    for event_id in &provenance_event_ids {
        validate_uuidv7(event_id)
            .map_err(|error| invalid_disk_id("skill provenance event id", event_id, error))?;
    }
    let scope_kind: String = row.get("scope_kind");
    let scope_companion_id: Option<String> = row.get("scope_companion_id");
    match (scope_kind.as_str(), scope_companion_id.as_deref()) {
        ("user", None) => {}
        ("companion", Some(owner)) => {
            CompanionId::try_from(owner)
                .map_err(|error| invalid_disk_id("skill scope companion id", owner, error))?;
        }
        _ => {
            return Err(AppError::Internal(format!(
                "companion store contains invalid skill scope: kind={scope_kind:?}, owner={scope_companion_id:?}"
            )));
        }
    }
    let skill_pattern_id: Option<String> = row.get("skill_pattern_id");
    if let Some(skill_pattern_id) = skill_pattern_id.as_deref() {
        validate_uuidv7(skill_pattern_id)
            .map_err(|error| invalid_disk_id("skill pattern id", skill_pattern_id, error))?;
    }
    Ok(CompanionSkill {
        companion_skill_id,
        skill_name: row.get("skill_name"),
        scope_kind,
        scope_companion_id,
        status: row.get("status"),
        source: row.get("source"),
        confidence: row.get("confidence"),
        provenance_event_ids,
        strength: row.get("strength"),
        version: row.get("version"),
        skill_pattern_id,
        usage_count: row.get("usage_count"),
        last_used_at: row.get("last_used_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        signature: row.get("signature"),
    })
}

impl CompanionStore {
    /// Read every durable skill row for the startup filesystem inventory audit.
    pub(crate) async fn list_all_skills(&self) -> Result<Vec<CompanionSkill>, AppError> {
        let rows = sqlx::query("SELECT * FROM companion_skills ORDER BY id ASC")
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.iter().map(row_to_skill).collect()
    }

    /// Insert or update a skill registry row by its durable business ID.
    pub async fn insert_skill(&self, s: &CompanionSkill) -> Result<(), AppError> {
        validate_uuidv7(&s.companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        match (s.scope_kind.as_str(), s.scope_companion_id.as_deref()) {
            ("user", None) => {}
            ("companion", Some(owner)) => validate_companion_id(owner, "skill scope companion_id")?,
            _ => {
                return Err(AppError::BadRequest(
                    "skill scope must be shared (user/None) or private (companion/Some(canonical ID))".into(),
                ));
            }
        }
        for event_id in &s.provenance_event_ids {
            validate_uuidv7(event_id).map_err(|error| {
                AppError::BadRequest(format!(
                    "invalid skill provenance_event_ids entry {event_id:?}: {error}"
                ))
            })?;
        }
        if let Some(skill_pattern_id) = s.skill_pattern_id.as_deref() {
            validate_uuidv7(skill_pattern_id)
                .map_err(|error| AppError::BadRequest(format!("invalid skill_pattern_id: {error}")))?;
            let parent_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM skill_pattern_stats WHERE skill_pattern_id = ?
                 )",
            )
            .bind(skill_pattern_id)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
            if !parent_exists {
                return Err(AppError::BadRequest(format!(
                    "skill_pattern_id '{skill_pattern_id}' does not reference an existing pattern"
                )));
            }
        }
        let provenance_event_ids = serde_json::to_string(&s.provenance_event_ids)
            .map_err(|error| AppError::BadRequest(format!("invalid skill provenance_event_ids: {error}")))?;
        sqlx::query(
            "INSERT INTO companion_skills(companion_skill_id, skill_name, scope_kind, scope_companion_id, status, source, confidence,
                provenance_event_ids, strength, version, skill_pattern_id, usage_count, last_used_at, created_at, updated_at, signature)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(companion_skill_id) DO UPDATE SET
                skill_name=excluded.skill_name, scope_kind=excluded.scope_kind,
                scope_companion_id=excluded.scope_companion_id,
                status=excluded.status, source=excluded.source, confidence=excluded.confidence,
                provenance_event_ids=excluded.provenance_event_ids, strength=excluded.strength,
                version=excluded.version, skill_pattern_id=excluded.skill_pattern_id,
                usage_count=excluded.usage_count, last_used_at=excluded.last_used_at,
                updated_at=excluded.updated_at, signature=excluded.signature",
        )
        .bind(&s.companion_skill_id)
        .bind(&s.skill_name)
        .bind(&s.scope_kind)
        .bind(&s.scope_companion_id)
        .bind(&s.status)
        .bind(&s.source)
        .bind(s.confidence)
        .bind(&provenance_event_ids)
        .bind(s.strength)
        .bind(s.version)
        .bind(&s.skill_pattern_id)
        .bind(s.usage_count)
        .bind(s.last_used_at)
        .bind(s.created_at)
        .bind(s.updated_at)
        .bind(&s.signature)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// List a companion's own skills; with `include_shared`, also the user-scoped (shared) ones.
    pub async fn list_skills(&self, companion_id: &str, include_shared: bool) -> Result<Vec<CompanionSkill>, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let sql = if include_shared {
            "SELECT * FROM companion_skills WHERE scope_companion_id = ? OR scope_kind = 'user' \
             ORDER BY strength DESC, updated_at DESC, scope_kind ASC, scope_companion_id ASC, skill_name ASC"
        } else {
            "SELECT * FROM companion_skills WHERE scope_companion_id = ? \
             ORDER BY strength DESC, updated_at DESC, scope_kind ASC, scope_companion_id ASC, skill_name ASC"
        };
        let rows = sqlx::query(sql).bind(companion_id).fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_skill).collect()
    }

    /// List one page of skills visible to a companion, optionally limited to one lifecycle status.
    pub async fn list_skill_page(
        &self,
        companion_id: &str,
        include_shared: bool,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<CompanionSkillPage, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let scope_clause = if include_shared {
            " WHERE (scope_companion_id = ? OR scope_kind = 'user')"
        } else {
            " WHERE scope_companion_id = ?"
        };
        let status_clause = if status.is_some() { " AND status = ?" } else { "" };
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);

        let items_sql = format!(
            "SELECT * FROM companion_skills{scope_clause}{status_clause} \
             ORDER BY strength DESC, updated_at DESC, scope_kind ASC, scope_companion_id ASC, skill_name ASC LIMIT ? OFFSET ?"
        );
        let mut items_query = sqlx::query(&items_sql).bind(companion_id);
        if let Some(status) = status {
            items_query = items_query.bind(status);
        }
        let rows = items_query
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        let count_sql = format!("SELECT COUNT(*) AS n FROM companion_skills{scope_clause}{status_clause}");
        let mut count_query = sqlx::query(&count_sql).bind(companion_id);
        if let Some(status) = status {
            count_query = count_query.bind(status);
        }
        let total = count_query.fetch_one(&self.pool).await.map_err(db_err)?.get("n");

        Ok(CompanionSkillPage {
            items: rows.iter().map(row_to_skill).collect::<Result<Vec<_>, _>>()?,
            total,
        })
    }

    pub async fn get_skill(&self, companion_skill_id: &str) -> Result<Option<CompanionSkill>, AppError> {
        validate_uuidv7(companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        let row = sqlx::query("SELECT * FROM companion_skills WHERE companion_skill_id = ?")
            .bind(companion_skill_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.as_ref().map(row_to_skill).transpose()
    }

    pub async fn get_owned_skill(
        &self,
        companion_id: &str,
        companion_skill_id: &str,
    ) -> Result<Option<CompanionSkill>, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        validate_uuidv7(companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        let row = sqlx::query(
            "SELECT * FROM companion_skills
             WHERE scope_companion_id = ? AND companion_skill_id = ?",
        )
            .bind(companion_id)
            .bind(companion_skill_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.as_ref().map(row_to_skill).transpose()
    }

    pub async fn find_owned_skill_by_name(
        &self,
        companion_id: &str,
        skill_name: &str,
    ) -> Result<Option<CompanionSkill>, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let row = sqlx::query(
            "SELECT * FROM companion_skills
             WHERE scope_companion_id = ? AND skill_name = ?",
        )
        .bind(companion_id)
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.as_ref().map(row_to_skill).transpose()
    }

    pub async fn set_skill_status(
        &self,
        companion_skill_id: &str,
        status: &str,
    ) -> Result<(), AppError> {
        validate_uuidv7(companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        let result = sqlx::query(
            "UPDATE companion_skills SET status = ?, updated_at = ?
             WHERE companion_skill_id = ?",
        )
            .bind(status)
            .bind(now_ms())
            .bind(companion_skill_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "companion skill '{companion_skill_id}' not found"
            )));
        }
        Ok(())
    }

    async fn record_skill_usage(
        &self,
        companion_skill_id: &str,
        now: i64,
    ) -> Result<(), AppError> {
        validate_uuidv7(companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        // Bump usage AND reinforce strength toward 1.0 (mirrors reinforce_memories) so that
        // a frequently-used skill survives the decay pass — "used skills stay sharp".
        sqlx::query(
            "UPDATE companion_skills SET usage_count = usage_count + 1, last_used_at = ?, \
             strength = MIN(1.0, strength + 0.1), updated_at = ? \
             WHERE companion_skill_id = ?",
        )
        .bind(now)
        .bind(now)
        .bind(companion_skill_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Resolve the runtime tool's human-readable name to one durable row, then
    /// perform the durable update by `companion_skill_id`.
    pub async fn record_skill_usage_by_name(
        &self,
        scope_companion_id: Option<&str>,
        skill_name: &str,
        now: i64,
    ) -> Result<(), AppError> {
        if let Some(companion_id) = scope_companion_id {
            validate_companion_id(companion_id, "skill scope companion_id")?;
        }
        let companion_skill_id: Option<String> = sqlx::query_scalar(
            "SELECT companion_skill_id FROM companion_skills
             WHERE ((? IS NULL AND scope_companion_id IS NULL) OR scope_companion_id = ?)
               AND skill_name = ?",
        )
        .bind(scope_companion_id)
        .bind(scope_companion_id)
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        if let Some(companion_skill_id) = companion_skill_id {
            self.record_skill_usage(&companion_skill_id, now).await?;
        }
        Ok(())
    }

    /// Decay active-skill strength by age since last use; auto-archive those below threshold.
    /// Manual/demonstrated skills (`source != 'mined'`) never decay (analog of profile memories).
    /// This is NOT a user rejection: it writes no feedback and never suppresses the originating
    /// pattern, so resumed behavior can be re-mined. Only flips the DB row (SKILL.md stays). Returns archived count.
    pub async fn decay_skills(
        &self,
        half_life_days: f64,
        archive_threshold: f64,
    ) -> Result<Vec<CompanionSkill>, AppError> {
        let now = now_ms();
        let rows = sqlx::query(
            "SELECT companion_skill_id, scope_companion_id, skill_name, source, strength,
                    COALESCE(last_used_at, created_at) AS clock \
             FROM companion_skills WHERE status = 'active'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let half = half_life_days.max(0.1);
        let mut archived = Vec::new();
        for row in rows {
            let source: String = row.get("source");
            if source != "mined" {
                continue; // manual / demonstrated / gifted skills never decay
            }
            let strength: f64 = row.get("strength");
            let clock: i64 = row.get("clock");
            let age_days = ((now - clock).max(0)) as f64 / 86_400_000.0;
            let decayed = strength * 0.5f64.powf(age_days / half);
            let cid: Option<String> = row.get("scope_companion_id");
            if let Some(companion_id) = cid.as_deref() {
                CompanionId::try_from(companion_id)
                    .map_err(|error| invalid_disk_id("skill scope companion id", companion_id, error))?;
            }
            let companion_skill_id: String = row.get("companion_skill_id");
            validate_uuidv7(&companion_skill_id).map_err(|error| {
                invalid_disk_id("companion skill id", &companion_skill_id, error)
            })?;
            if decayed < archive_threshold {
                sqlx::query(
                    "UPDATE companion_skills
                     SET strength = ?, status = 'archived', updated_at = ?
                     WHERE companion_skill_id = ?",
                )
                    .bind(decayed)
                    .bind(now)
                    .bind(&companion_skill_id)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
                let archived_row = sqlx::query(
                    "SELECT * FROM companion_skills WHERE companion_skill_id = ?",
                )
                .bind(&companion_skill_id)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
                archived.push(row_to_skill(&archived_row)?);
            } else {
                sqlx::query(
                    "UPDATE companion_skills SET strength = ? WHERE companion_skill_id = ?",
                )
                    .bind(decayed)
                    .bind(&companion_skill_id)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
            }
        }
        Ok(archived)
    }

    /// Count a companion's own active skills (for the expertise badge).
    pub async fn count_active_skills(&self, companion_id: &str) -> Result<i64, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM companion_skills WHERE scope_companion_id = ? AND status = 'active'",
        )
        .bind(companion_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(n)
    }

    /// Count a companion's skills created since `since_ms` (optionally filtered by status) — weekly digest.
    pub async fn count_skills_since(&self, companion_id: &str, since_ms: i64, status: Option<&str>) -> Result<i64, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let n: i64 = match status {
            Some(s) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM companion_skills WHERE scope_companion_id = ? AND created_at >= ? AND status = ?",
            )
            .bind(companion_id)
            .bind(since_ms)
            .bind(s)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query_scalar(
                "SELECT COUNT(*) FROM companion_skills WHERE scope_companion_id = ? AND created_at >= ?",
            )
            .bind(companion_id)
            .bind(since_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?,
        };
        Ok(n)
    }

    /// Skill names created since `since_ms`, newest first (for the weekly digest list).
    pub async fn list_skill_names_since(&self, companion_id: &str, since_ms: i64, limit: i64) -> Result<Vec<String>, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let rows = sqlx::query(
            "SELECT skill_name FROM companion_skills WHERE scope_companion_id = ? AND created_at >= ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(companion_id)
        .bind(since_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.iter().map(|r| r.get::<String, _>("skill_name")).collect())
    }

    /// Count active memories created since `since_ms` (global; memory.db is cross-companion).
    pub async fn count_memories_since(&self, since_ms: i64) -> Result<i64, AppError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM companion_memories WHERE status = 'active' AND created_at >= ?",
        )
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(n)
    }

    /// Find an existing active/draft skill of this companion whose NAME is near-identical to
    /// `name` (exact lowercased, or ≥0.6 containment) — for evolve-in-place instead of duplicating.
    /// Returns the durable skill row. Same-name is excluded because name
    /// collisions remain a filesystem constraint, not an entity identity.
    pub async fn find_similar_skill(
        &self,
        companion_id: &str,
        name: &str,
    ) -> Result<Option<CompanionSkill>, AppError> {
        validate_companion_id(companion_id, "skill companion_id")?;
        let target = name.to_lowercase();
        let rows = sqlx::query(
            "SELECT * FROM companion_skills
             WHERE scope_companion_id = ? AND status IN ('active','draft')",
        )
        .bind(companion_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        for row in &rows {
            let existing: String = row.get("skill_name");
            if existing == name {
                continue;
            }
            let e = existing.to_lowercase();
            if e == target {
                return row_to_skill(row).map(Some);
            }
            let (short, long) = if e.len() <= target.len() { (&e, &target) } else { (&target, &e) };
            if !short.is_empty() && long.contains(short.as_str()) && (short.len() as f64 / long.len() as f64) >= 0.6 {
                return row_to_skill(row).map(Some);
            }
        }
        Ok(None)
    }

    pub async fn find_skill_name_collision(
        &self,
        companion_id: &str,
        skill_name: &str,
    ) -> Result<Option<CompanionSkill>, AppError> {
        self.find_owned_skill_by_name(companion_id, skill_name).await
    }

    /// Bump a skill's version (on evolve-in-place).
    pub async fn bump_skill_version(&self, companion_skill_id: &str) -> Result<(), AppError> {
        validate_uuidv7(companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        sqlx::query(
            "UPDATE companion_skills SET version = version + 1, updated_at = ?
             WHERE companion_skill_id = ?",
        )
            .bind(now_ms())
            .bind(companion_skill_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Record one pattern occurrence and retain at most 50 fixed-structure
    /// samples. `distinct_sessions` is the count of distinct conversation IDs.
    pub async fn bump_pattern(
        &self,
        signature: &str,
        conversation_id: &str,
        event_id: &str,
        now: i64,
    ) -> Result<SkillPattern, AppError> {
        if signature.trim().is_empty() {
            return Err(AppError::BadRequest(
                "pattern signature must not be empty".into(),
            ));
        }
        let conversation_id = ConversationId::try_from(conversation_id)
            .map_err(|error| AppError::BadRequest(format!("invalid pattern conversation_id: {error}")))?;
        validate_uuidv7(event_id)
            .map_err(|error| AppError::BadRequest(format!("invalid pattern event_id: {error}")))?;
        let existing = sqlx::query(
            "SELECT skill_pattern_id, examples, status
             FROM skill_pattern_stats WHERE signature = ? ORDER BY id ASC LIMIT 1",
        )
            .bind(signature)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        let skill_pattern_id = existing
            .as_ref()
            .map(|row| row.get::<String, _>("skill_pattern_id"))
            .unwrap_or_else(nomifun_common::generate_id);
        validate_uuidv7(&skill_pattern_id).map_err(|error| {
            invalid_disk_id("skill pattern id", &skill_pattern_id, error)
        })?;
        let mut examples: Vec<PatternExample> = existing
            .as_ref()
            .map(|row| row.get::<String, _>("examples"))
            .as_deref()
            .map(|raw| {
                serde_json::from_str(raw).map_err(|error| {
                    AppError::Internal(format!(
                        "companion store pattern {signature:?} contains invalid examples JSON: {error}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or_default();
        examples.push(PatternExample {
            conversation_id,
            event_id: event_id.to_owned(),
        });
        if examples.len() > 50 {
            let cut = examples.len() - 50;
            examples.drain(0..cut);
        }
        let distinct: std::collections::HashSet<&str> =
            examples.iter().map(|sample| sample.conversation_id.as_str()).collect();
        let distinct_n = distinct.len() as i64;
        let examples_json = serde_json::to_string(&examples)
            .map_err(|error| AppError::Internal(format!("serialize pattern examples: {error}")))?;
        if existing.is_some() {
            sqlx::query(
                "UPDATE skill_pattern_stats
                 SET occurrence_count = occurrence_count + 1,
                     distinct_sessions = ?, examples = ?, last_seen = ?
                 WHERE skill_pattern_id = ?",
            )
            .bind(distinct_n)
            .bind(&examples_json)
            .bind(now)
            .bind(&skill_pattern_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        } else {
            sqlx::query(
                "INSERT INTO skill_pattern_stats(
                    skill_pattern_id, signature, occurrence_count,
                    distinct_sessions, examples, status, last_seen
                 ) VALUES(?,?,1,?,?,'open',?)",
            )
            .bind(&skill_pattern_id)
            .bind(signature)
            .bind(distinct_n)
            .bind(&examples_json)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        }
        Ok(SkillPattern {
            skill_pattern_id,
            signature: signature.to_owned(),
            status: existing
                .as_ref()
                .map(|row| row.get("status"))
                .unwrap_or_else(|| "open".to_owned()),
        })
    }

    pub async fn mark_pattern_status(
        &self,
        skill_pattern_id: &str,
        status: &str,
    ) -> Result<(), AppError> {
        validate_uuidv7(skill_pattern_id)
            .map_err(|error| AppError::BadRequest(format!("invalid skill_pattern_id: {error}")))?;
        let result = sqlx::query(
            "UPDATE skill_pattern_stats SET status = ? WHERE skill_pattern_id = ?",
        )
            .bind(status)
            .bind(skill_pattern_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "skill pattern '{skill_pattern_id}' not found"
            )));
        }
        Ok(())
    }

    /// Resolve a derived signature to the durable pattern identity.
    pub async fn find_pattern_by_signature(
        &self,
        signature: &str,
    ) -> Result<Option<SkillPattern>, AppError> {
        let row = sqlx::query(
            "SELECT skill_pattern_id, signature, status
             FROM skill_pattern_stats WHERE signature = ? ORDER BY id ASC LIMIT 1",
        )
            .bind(signature)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.map(|row| {
            let skill_pattern_id: String = row.get("skill_pattern_id");
            validate_uuidv7(&skill_pattern_id).map_err(|error| {
                invalid_disk_id("skill pattern id", &skill_pattern_id, error)
            })?;
            Ok(SkillPattern {
                skill_pattern_id,
                signature: row.get("signature"),
                status: row.get("status"),
            })
        })
        .transpose()
    }

    pub async fn record_feedback(
        &self,
        feedback_id: &str,
        companion_skill_id: &str,
        skill_name_snapshot: &str,
        skill_pattern_id: Option<&str>,
        signature_snapshot: Option<&str>,
        decision: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<(), AppError> {
        nomifun_common::CompanionEvolutionFeedbackId::try_from(feedback_id).map_err(|error| {
            AppError::BadRequest(format!("invalid evolution feedback id: {error}"))
        })?;
        validate_uuidv7(companion_skill_id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion_skill_id: {error}")))?;
        if let Some(skill_pattern_id) = skill_pattern_id {
            validate_uuidv7(skill_pattern_id)
                .map_err(|error| AppError::BadRequest(format!("invalid skill_pattern_id: {error}")))?;
        }
        if skill_name_snapshot.trim().is_empty() {
            return Err(AppError::BadRequest(
                "evolution feedback skill_name_snapshot must not be empty".into(),
            ));
        }
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let skill_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM companion_skills WHERE companion_skill_id = ?
             )",
        )
        .bind(companion_skill_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        if !skill_exists {
            return Err(AppError::BadRequest(format!(
                "companion_skill_id '{companion_skill_id}' does not reference an existing skill"
            )));
        }
        if let Some(skill_pattern_id) = skill_pattern_id {
            let pattern_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM skill_pattern_stats WHERE skill_pattern_id = ?
                 )",
            )
            .bind(skill_pattern_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if !pattern_exists {
                return Err(AppError::BadRequest(format!(
                    "skill_pattern_id '{skill_pattern_id}' does not reference an existing pattern"
                )));
            }
        }
        sqlx::query(
            "INSERT INTO evolution_feedback(
                feedback_id, companion_skill_id, skill_name_snapshot,
                skill_pattern_id, signature_snapshot, decision, reason, created_at
             ) VALUES(?,?,?,?,?,?,?,?)",
        )
            .bind(feedback_id)
            .bind(companion_skill_id)
            .bind(skill_name_snapshot)
            .bind(skill_pattern_id)
            .bind(signature_snapshot)
            .bind(decision)
            .bind(reason)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    /// 是否曾被拒绝（负样本）：存在 decision='reject' 的反馈即视为该签名被否决。
    pub async fn is_signature_rejected(&self, signature: &str) -> Result<bool, AppError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*)
             FROM evolution_feedback f
             LEFT JOIN skill_pattern_stats p
               ON p.skill_pattern_id = f.skill_pattern_id
             WHERE (p.signature = ? OR f.signature_snapshot = ?)
               AND f.decision = 'reject'",
        )
            .bind(signature)
            .bind(signature)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn companion_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        CompanionId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn conversation_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        ConversationId::try_from(raw.as_str()).unwrap().into_string()
    }

    #[tokio::test]
    async fn v3_baseline_all_tables_use_autoincrement_integer_primary_keys() {
        let store = CompanionStore::open_memory().await.unwrap();
        validate_baseline_schema(&store.pool).await.unwrap();
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_VERSION);

        for table in BASELINE_TABLES {
            let columns = sqlx::query(&format!("PRAGMA table_info({})", table.name))
                .fetch_all(&store.pool)
                .await
                .unwrap();
            let id = columns
                .iter()
                .find(|row| row.get::<String, _>("name") == "id")
                .unwrap();
            assert_eq!(id.get::<String, _>("type").to_ascii_uppercase(), "INTEGER");
            assert_eq!(id.get::<i64, _>("pk"), 1);
        }
    }

    async fn assert_malformed_v3_rejected(schema: &str, description: &str) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap();
        sqlx::raw_sql(schema).execute(&pool).await.unwrap();
        sqlx::raw_sql(&format!("PRAGMA user_version = {STORE_VERSION}"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            validate_baseline_schema(&pool).await.is_err(),
            "malformed v3 schema must be rejected: {description}"
        );
    }

    #[tokio::test]
    async fn v3_baseline_rejects_missing_columns_uuid_checks_uniques_and_indexes() {
        let malformed = [
            (
                SCHEMA.replacen("  action TEXT,\n", "", 1),
                "all tables exist but companion_suggestions.action is missing",
            ),
            (
                SCHEMA.replacen("  action TEXT,\n", "  action INTEGER,\n", 1),
                "companion_suggestions.action has the wrong declared type",
            ),
            (
                SCHEMA.replacen("  action TEXT,\n", "  action TEXT NOT NULL,\n", 1),
                "companion_suggestions.action has the wrong nullability",
            ),
            (
                SCHEMA.replacen(
                    "  decided_at INTEGER\n);",
                    "  decided_at INTEGER,\n  unexpected TEXT\n);",
                    1,
                ),
                "companion_suggestions has an extra column",
            ),
            (
                SCHEMA.replacen(
                    "suggestion_id TEXT NOT NULL UNIQUE CHECK (\n    length(suggestion_id) = 36\n    AND lower(suggestion_id) = suggestion_id\n    AND suggestion_id GLOB '????????-????-7???-[89ab]???-????????????'\n    AND replace(suggestion_id, '-', '') NOT GLOB '*[^0-9a-f]*'\n  )",
                    "suggestion_id TEXT NOT NULL UNIQUE",
                    1,
                ),
                "suggestion_id has no UUIDv7 CHECK",
            ),
            (
                SCHEMA.replacen(
                    "suggestion_id TEXT NOT NULL UNIQUE CHECK",
                    "suggestion_id TEXT NOT NULL CHECK",
                    1,
                ),
                "suggestion_id has no UNIQUE constraint",
            ),
            (
                SCHEMA.replacen(
                    "CREATE INDEX IF NOT EXISTS idx_companion_suggestions_status ON companion_suggestions(status, created_at DESC);\n",
                    "",
                    1,
                ),
                "required suggestion status index is missing",
            ),
            (
                SCHEMA.replacen(
                    "CREATE UNIQUE INDEX IF NOT EXISTS idx_companion_skills_shared_name ON companion_skills(skill_name) WHERE scope_kind = 'user';\n",
                    "",
                    1,
                ),
                "required partial unique skill index is missing",
            ),
            (
                format!(
                    "{SCHEMA}\nCREATE INDEX unexpected_v3_index ON companion_suggestions(kind);"
                ),
                "an extra user-defined index is present",
            ),
        ];

        for (schema, description) in malformed {
            assert_malformed_v3_rejected(&schema, description).await;
        }
    }

    #[tokio::test]
    async fn v3_baseline_rejects_non_v3_table_shape() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE companion_memories (id TEXT PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(create_baseline_schema(&pool).await.is_err());
    }

    #[tokio::test]
    async fn file_store_rejects_unversioned_or_future_schema_without_repair() {
        for version in [0_i64, STORE_VERSION + 1] {
            let root = tempfile::tempdir().unwrap();
            let database_path = root.path().join("memory.db");
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(&database_path)
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
            sqlx::raw_sql(&format!("PRAGMA user_version = {version}"))
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;

            assert!(CompanionStore::open(root.path()).await.is_err());
            let verify = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(SqliteConnectOptions::new().filename(&database_path))
                .await
                .unwrap();
            let persisted: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&verify)
                .await
                .unwrap();
            assert_eq!(
                persisted, version,
                "open must not stamp a non-v3 store as v3"
            );
            verify.close().await;
        }
    }

    #[tokio::test]
    async fn state_and_runtime_state_upsert_by_unique_business_keys() {
        let store = CompanionStore::open_memory().await.unwrap();
        store.set_state("cursor", "1").await.unwrap();
        store.set_state("cursor", "2").await.unwrap();
        assert_eq!(store.get_state("cursor").await.unwrap().as_deref(), Some("2"));

        let companion = companion_fixture(1);
        store.set_companion_state(&companion, "mood", "ok").await.unwrap();
        store.set_companion_state(&companion, "mood", "happy").await.unwrap();
        assert_eq!(
            store.get_companion_state(&companion, "mood").await.unwrap().as_deref(),
            Some("happy")
        );
    }

    #[tokio::test]
    async fn memory_suggestion_and_learn_run_use_named_unique_ids() {
        let store = CompanionStore::open_memory().await.unwrap();
        let memory = store
            .insert_memory("knowledge", "Rust", &[], 0.8, "manual")
            .await
            .unwrap();
        assert_eq!(
            store.get_memory(&memory.memory_id).await.unwrap().unwrap().memory_id,
            memory.memory_id
        );

        let suggestion = store.insert_suggestion("insight", "标题", "正文", None).await.unwrap();
        let (decided, newly_decided) = store
            .decide_suggestion(&suggestion.suggestion_id, true)
            .await
            .unwrap();
        assert!(newly_decided);
        assert_eq!(decided.suggestion_id, suggestion.suggestion_id);

        let run = CompanionLearnRun {
            learn_run_id: CompanionLearnRunId::new().into_string(),
            started_at: 1,
            finished_at: Some(2),
            status: "ok".into(),
            events_processed: 1,
            memories_added: 1,
            suggestions_added: 1,
            error: None,
            summary: None,
        };
        store.insert_learn_run(&run).await.unwrap();
        assert!(store.learn_run_exists(&run.learn_run_id).await.unwrap());
    }

    #[tokio::test]
    async fn pattern_examples_persist_event_id_and_reject_generic_id() {
        let store = CompanionStore::open_memory().await.unwrap();
        let conversation_id = conversation_fixture(20);
        let event_id = nomifun_common::generate_id();
        store
            .bump_pattern("grep-read", &conversation_id, &event_id, 1)
            .await
            .unwrap();

        let examples: String =
            sqlx::query_scalar("SELECT examples FROM skill_pattern_stats WHERE signature = ?")
                .bind("grep-read")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        let wire: serde_json::Value = serde_json::from_str(&examples).unwrap();
        assert_eq!(wire[0]["event_id"], event_id);
        assert!(wire[0].get("id").is_none());

        sqlx::query("UPDATE skill_pattern_stats SET examples = ? WHERE signature = ?")
            .bind(format!(
                "[{{\"conversation_id\":\"{conversation_id}\",\"id\":\"{}\"}}]",
                nomifun_common::generate_id()
            ))
            .bind("grep-read")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store
                .bump_pattern(
                    "grep-read",
                    &conversation_id,
                    &nomifun_common::generate_id(),
                    2
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn thread_and_session_window_roundtrip() {
        let store = CompanionStore::open_memory().await.unwrap();
        let companion = companion_fixture(1);
        let conversation = conversation_fixture(1);
        let thread = store
            .insert_companion_thread(&conversation, &companion, "伙伴会话")
            .await
            .unwrap();
        assert_eq!(thread.conversation_id, conversation);

        let window = store.ensure_open_window(&companion, &conversation, 0).await.unwrap();
        store.touch_window(&window.session_window_id, 10, 2).await.unwrap();
        store.close_window(&window.session_window_id, "archived", Some("摘要"), None, 8).await.unwrap();
        let digests = store.list_digests(&companion, 10).await.unwrap();
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].session_window_id, window.session_window_id);
    }
}
