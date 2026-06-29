//! The companion's dedicated sqlite store (`{companion_dir}/memory.db`): memories,
//! suggestions, companion-chat history, learn-run history, and a small
//! key-value state table (xp/mood/cursor/rolling chat summary).
//!
//! Deliberately a separate db file from the main app database so "clear all
//! companion data" stays a file-scoped operation and companion writes never contend with
//! conversation traffic.

use std::path::{Path, PathBuf};

use nomifun_common::{AppError, TimestampMs, generate_prefixed_id, now_ms};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqliteConnection, SqlitePool};

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

/// serde default for [`CompanionMemory::scope_kind`] so legacy export bundles
/// (written before scope existed) deserialize as the shared/global default.
fn default_scope_kind() -> String {
    "user".to_string()
}

/// Visibility of a companion memory. Mirrors the companion-skills scoping
/// (`scope_kind` `'user'`=shared / `'companion'`=private + `scope_companion_id`
/// `''`=shared). Shared memories inject/recall for every companion; private
/// memories only for their owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryScope {
    /// Cross-companion: visible to every companion (legacy default).
    Shared,
    /// Owned by one companion: visible only to it.
    Companion(String),
}

impl MemoryScope {
    /// `(scope_kind, scope_companion_id)` column values.
    pub fn columns(&self) -> (&'static str, String) {
        match self {
            MemoryScope::Shared => ("user", String::new()),
            MemoryScope::Companion(id) => ("companion", id.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionMemory {
    pub id: String,
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
    #[serde(default = "default_scope_kind")]
    pub scope_kind: String,
    /// Owning companion id when private; `''` when shared.
    #[serde(default)]
    pub scope_companion_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionSuggestion {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    /// Optional UI action, e.g. `{"type":"navigate","to":"/scheduled"}`.
    pub action: Option<serde_json::Value>,
    pub status: String,
    pub created_at: TimestampMs,
    pub decided_at: Option<TimestampMs>,
}


/// One registered companion chat thread (a real `type='nomi'` conversation
/// owned by the main conversation domain; the companion only tracks membership).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionThread {
    pub conversation_id: String,
    /// Owning companion (`companion_…`). Empty string = legacy row not yet backfilled.
    pub companion_id: String,
    pub title: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionLearnRun {
    pub id: String,
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

#[derive(Clone)]
pub struct CompanionStore {
    pool: SqlitePool,
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
/// this process. `None` means boot fell back to the in-memory store (corrupt
/// or locked `memory.db`) — callers should refuse export/import rather than
/// operate on a throwaway snapshot.
pub fn live_store() -> Option<(&'static Path, &'static CompanionStore)> {
    LIVE_STORE.get().map(|(dir, store)| (dir.as_path(), store))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS companion_memories (
  id TEXT PRIMARY KEY,
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
  scope_kind TEXT NOT NULL DEFAULT 'user',
  scope_companion_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_companion_memories_kind ON companion_memories(kind, status, strength DESC);

CREATE TABLE IF NOT EXISTS companion_suggestions (
  id TEXT PRIMARY KEY,
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
  id TEXT PRIMARY KEY,
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
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS companion_threads (
  conversation_id TEXT PRIMARY KEY,
  companion_id TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS companion_runtime_state (
  companion_id TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY(companion_id, key)
);

CREATE TABLE IF NOT EXISTS companion_skills (
  skill_name TEXT NOT NULL,
  scope_kind TEXT NOT NULL DEFAULT 'companion',
  scope_companion_id TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'draft',
  source TEXT NOT NULL DEFAULT 'mined',
  confidence REAL NOT NULL DEFAULT 0.0,
  provenance TEXT NOT NULL DEFAULT '[]',
  strength REAL NOT NULL DEFAULT 1.0,
  version INTEGER NOT NULL DEFAULT 1,
  superseded_by TEXT,
  usage_count INTEGER NOT NULL DEFAULT 0,
  last_used_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  signature TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(scope_kind, scope_companion_id, skill_name)
);
CREATE INDEX IF NOT EXISTS idx_companion_skills_owner ON companion_skills(scope_companion_id, status, strength DESC);

CREATE TABLE IF NOT EXISTS skill_pattern_stats (
  signature TEXT PRIMARY KEY,
  count INTEGER NOT NULL DEFAULT 0,
  distinct_sessions INTEGER NOT NULL DEFAULT 0,
  example_event_ids TEXT NOT NULL DEFAULT '[]',
  status TEXT NOT NULL DEFAULT 'open',
  last_seen INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evolution_feedback (
  id TEXT PRIMARY KEY,
  draft_id TEXT NOT NULL,
  signature TEXT,
  decision TEXT NOT NULL,
  reason TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_evolution_feedback_sig ON evolution_feedback(signature);
"#;

fn db_err(e: sqlx::Error) -> AppError {
    AppError::Internal(format!("companion store: {e}"))
}

/// 桌面伙伴单会话不变式：每个伙伴最多一条 companion 会话。空 companion_id（未回填的
/// 遗留行）允许并存，故索引带 `WHERE companion_id != ''` 谓词放过它们。Created for
/// fresh dbs in [`init_schema`] (after the table is born with `companion_id`) and for
/// pre-existing dbs by the v1→v2 migration. NOT part of the inline SCHEMA: that
/// string also runs against pre-v1 tables that still lack the `companion_id` column,
/// where referencing it would error.
const COMPANION_UNIQUE_INDEX: &str = "CREATE UNIQUE INDEX IF NOT EXISTS idx_companion_threads_companion \
     ON companion_threads(companion_id) WHERE companion_id != ''";

/// Current schema version stamped into `PRAGMA user_version`. The base
/// SCHEMA always reflects this latest shape (fresh dbs are born current).
const STORE_VERSION: i64 = 4;

/// Schema bootstrap shared by `open`/`open_memory`. Runs entirely on one
/// acquired connection so DDL is never spread across pool members. Probes
/// whether the db is brand new *before* running SCHEMA (no `companion_*` table at
/// all = fresh): fresh dbs get the SCHEMA and are stamped [`STORE_VERSION`]
/// directly, skipping every migration rung; pre-existing dbs get the SCHEMA
/// (a no-op on their old tables) and then walk [`apply_migrations_on`].
/// One-shot legacy rename: a `memory.db` created under the old "pet" naming
/// carries `pet_*` tables (and `pet_id` columns). Rename them to the
/// `companion_*` schema the current code expects, BEFORE [`init_schema`]'s
/// fresh-vs-existing probe — otherwise an old db (no `companion_*` tables yet)
/// reads as "fresh" and gets empty `companion_*` tables built alongside the
/// orphaned `pet_*` data. Idempotent: every rename is guarded by an existence
/// check, so fresh dbs and already-migrated dbs are no-ops.
async fn normalize_legacy_pet_schema(conn: &mut SqliteConnection) -> Result<(), AppError> {
    async fn table_exists(conn: &mut SqliteConnection, name: &str) -> Result<bool, AppError> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(name)
            .fetch_one(&mut *conn)
            .await
            .map_err(db_err)?;
        Ok(n > 0)
    }
    async fn column_exists(conn: &mut SqliteConnection, table: &str, col: &str) -> Result<bool, AppError> {
        Ok(sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?
            .iter()
            .any(|row| row.get::<String, _>("name") == col))
    }
    const TABLE_RENAMES: &[(&str, &str)] = &[
        ("pet_memories", "companion_memories"),
        ("pet_suggestions", "companion_suggestions"),
        ("pet_learn_runs", "companion_learn_runs"),
        ("pet_state", "companion_state"),
        ("pet_companion_threads", "companion_threads"),
        ("pet_runtime_state", "companion_runtime_state"),
    ];
    for (old, new) in TABLE_RENAMES {
        if table_exists(conn, old).await? && !table_exists(conn, new).await? {
            sqlx::raw_sql(&format!("ALTER TABLE {old} RENAME TO {new}"))
                .execute(&mut *conn)
                .await
                .map_err(db_err)?;
        }
    }
    for tbl in ["companion_threads", "companion_runtime_state"] {
        if table_exists(conn, tbl).await?
            && column_exists(conn, tbl, "pet_id").await?
            && !column_exists(conn, tbl, "companion_id").await?
        {
            sqlx::raw_sql(&format!("ALTER TABLE {tbl} RENAME COLUMN pet_id TO companion_id"))
                .execute(&mut *conn)
                .await
                .map_err(db_err)?;
        }
    }
    Ok(())
}

async fn init_schema(pool: &SqlitePool) -> Result<(), AppError> {
    let mut conn = pool.acquire().await.map_err(db_err)?;
    // Carry any legacy `pet_*` schema forward to `companion_*` before the
    // fresh-vs-existing probe below (see [`normalize_legacy_pet_schema`]).
    normalize_legacy_pet_schema(&mut conn).await?;
    // Fresh probe: any surviving companion table marks a pre-existing db, not just
    // companion_threads (a partially created old db must still walk the
    // migration ladder instead of being stamped current). `\_` keeps the
    // LIKE underscore literal.
    let existing_tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name LIKE 'companion\\_%' ESCAPE '\\'",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    sqlx::raw_sql(SCHEMA).execute(&mut *conn).await.map_err(db_err)?;
    if existing_tables == 0 {
        // Fresh db: the table was just born with `companion_id`, so the single-
        // session unique index is safe to create now (and fresh dbs skip the
        // migration ladder that would otherwise create it).
        sqlx::raw_sql(COMPANION_UNIQUE_INDEX).execute(&mut *conn).await.map_err(db_err)?;
        sqlx::raw_sql(&format!("PRAGMA user_version = {STORE_VERSION}"))
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;
    } else {
        apply_migrations_on(&mut conn).await?;
    }
    Ok(())
}

/// Versioned migration ladder for databases created before the current
/// SCHEMA, driven by `PRAGMA user_version`. Fresh dbs never get here — the
/// [`init_schema`] dispatcher stamps them [`STORE_VERSION`] directly. Each
/// rung preflights the actual shape (e.g. `PRAGMA table_info`) instead of
/// sniffing error messages, so it stays idempotent.
///
/// Test-only pool entry point; production goes through [`init_schema`],
/// which already holds a connection and calls [`apply_migrations_on`].
#[cfg(test)]
async fn apply_migrations(pool: &SqlitePool) -> Result<(), AppError> {
    let mut conn = pool.acquire().await.map_err(db_err)?;
    apply_migrations_on(&mut conn).await
}

/// The actual ladder, pinned to one connection. Each rung runs inside
/// `BEGIN IMMEDIATE` so preflight + ALTER + version stamp are atomic and a
/// concurrent second process serializes on the write lock instead of racing
/// the ALTER (which would error and bounce that process to a memory db).
async fn apply_migrations_on(conn: &mut SqliteConnection) -> Result<(), AppError> {
    let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
    if version < 1 {
        sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut *conn).await.map_err(db_err)?;
        match migrate_v0_to_v1(conn).await {
            Ok(()) => {
                sqlx::raw_sql("COMMIT").execute(&mut *conn).await.map_err(db_err)?;
            }
            Err(e) => {
                let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                return Err(e);
            }
        }
    }
    if version < 2 {
        sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut *conn).await.map_err(db_err)?;
        match migrate_v1_to_v2(conn).await {
            Ok(()) => {
                sqlx::raw_sql("COMMIT").execute(&mut *conn).await.map_err(db_err)?;
            }
            Err(e) => {
                let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                return Err(e);
            }
        }
    }
    if version < 3 {
        sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut *conn).await.map_err(db_err)?;
        match migrate_v2_to_v3(conn).await {
            Ok(()) => {
                sqlx::raw_sql("COMMIT").execute(&mut *conn).await.map_err(db_err)?;
            }
            Err(e) => {
                let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                return Err(e);
            }
        }
    }
    if version < 4 {
        sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut *conn).await.map_err(db_err)?;
        match migrate_v3_to_v4(conn).await {
            Ok(()) => {
                sqlx::raw_sql("COMMIT").execute(&mut *conn).await.map_err(db_err)?;
            }
            Err(e) => {
                let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                return Err(e);
            }
        }
    }
    Ok(())
}

/// v0 → v1: companion_threads grows a companion_id column. Only ALTER when
/// table_info says the column is genuinely missing (the transaction holds
/// the write lock, so the preflight cannot go stale before the ALTER).
async fn migrate_v0_to_v1(conn: &mut SqliteConnection) -> Result<(), AppError> {
    let has_companion_id = sqlx::query("PRAGMA table_info(companion_threads)")
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?
        .iter()
        .any(|row| row.get::<String, _>("name") == "companion_id");
    if !has_companion_id {
        sqlx::raw_sql("ALTER TABLE companion_threads ADD COLUMN companion_id TEXT NOT NULL DEFAULT ''")
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;
    }
    sqlx::raw_sql("PRAGMA user_version = 1").execute(&mut *conn).await.map_err(db_err)?;
    Ok(())
}

/// v1 → v2: enforce the work-partner single-session invariant — at most one
/// companion thread per companion. Legacy rows may carry duplicate `companion_id`s, so we
/// dedupe FIRST (keep the most-recently-updated thread per companion, delete the
/// rest from `companion_threads` only — never touch conversations or
/// companion_memories) and only THEN create the partial UNIQUE INDEX. Empty companion_id
/// (un-backfilled legacy rows) is exempt from both the dedupe and the index.
/// Crash-safe/idempotent: `CREATE UNIQUE INDEX IF NOT EXISTS`, and re-running
/// the dedupe DELETE on an already-deduped table is a no-op.
async fn migrate_v1_to_v2(conn: &mut SqliteConnection) -> Result<(), AppError> {
    // Dedupe: within each non-empty companion_id, keep the row with the largest
    // updated_at (ties broken by the larger conversation_id so the choice is
    // deterministic). Delete a row when some OTHER row for the same companion ranks
    // strictly higher by (updated_at, conversation_id). Registry rows only —
    // the backing conversations + shared memories are untouched.
    sqlx::raw_sql(
        "DELETE FROM companion_threads
         WHERE companion_id != ''
           AND EXISTS (
             SELECT 1 FROM companion_threads b
             WHERE b.companion_id = companion_threads.companion_id
               AND (b.updated_at > companion_threads.updated_at
                    OR (b.updated_at = companion_threads.updated_at
                        AND b.conversation_id > companion_threads.conversation_id))
           )",
    )
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    sqlx::raw_sql(COMPANION_UNIQUE_INDEX)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    sqlx::raw_sql("PRAGMA user_version = 2").execute(&mut *conn).await.map_err(db_err)?;
    Ok(())
}

/// v2 → v3: companion_memories 增加分层范围维度（scope_kind/scope_companion_id）。
/// 旧行默认 scope_kind='user'（全员共享，维持现状语义）。先 table_info 预检，缺列才 ALTER，
/// 故对已含列的库幂等。事务由 [`apply_migrations_on`] 的 BEGIN IMMEDIATE 包裹。
async fn migrate_v2_to_v3(conn: &mut SqliteConnection) -> Result<(), AppError> {
    // companion_memories 可能尚未建（极老的库，或测试直连 apply_migrations 而未先跑 SCHEMA）。
    // 只有表存在且缺列时才 ALTER；生产路径里 SCHEMA 先于迁移运行，表必然存在（已含列则全 no-op）。
    let table_present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='companion_memories'",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if table_present > 0 {
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(companion_memories)")
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?
            .iter()
            .map(|row| row.get::<String, _>("name"))
            .collect();
        if !cols.iter().any(|c| c == "scope_kind") {
            sqlx::raw_sql("ALTER TABLE companion_memories ADD COLUMN scope_kind TEXT NOT NULL DEFAULT 'user'")
                .execute(&mut *conn)
                .await
                .map_err(db_err)?;
        }
        if !cols.iter().any(|c| c == "scope_companion_id") {
            sqlx::raw_sql("ALTER TABLE companion_memories ADD COLUMN scope_companion_id TEXT")
                .execute(&mut *conn)
                .await
                .map_err(db_err)?;
        }
    }
    sqlx::raw_sql("PRAGMA user_version = 3").execute(&mut *conn).await.map_err(db_err)?;
    Ok(())
}

/// v3 → v4: companion_skills grows a `signature` column (the originating mined-pattern
/// signature), so rejecting a skill can suppress its pattern from re-proposal (纠偏回流).
/// Preflight table + column existence; idempotent.
async fn migrate_v3_to_v4(conn: &mut SqliteConnection) -> Result<(), AppError> {
    let table_present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='companion_skills'",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if table_present > 0 {
        let has_signature = sqlx::query("PRAGMA table_info(companion_skills)")
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?
            .iter()
            .any(|row| row.get::<String, _>("name") == "signature");
        if !has_signature {
            sqlx::raw_sql("ALTER TABLE companion_skills ADD COLUMN signature TEXT NOT NULL DEFAULT ''")
                .execute(&mut *conn)
                .await
                .map_err(db_err)?;
        }
    }
    sqlx::raw_sql("PRAGMA user_version = 4").execute(&mut *conn).await.map_err(db_err)?;
    Ok(())
}

fn row_to_memory(row: &sqlx::sqlite::SqliteRow) -> CompanionMemory {
    let tags: String = row.get("tags");
    CompanionMemory {
        id: row.get("id"),
        kind: row.get("kind"),
        content: row.get("content"),
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        importance: row.get("importance"),
        strength: row.get("strength"),
        pinned: row.get::<i64, _>("pinned") != 0,
        source: row.get("source"),
        status: row.get("status"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_reinforced_at: row.get("last_reinforced_at"),
        // scope_kind is NOT NULL DEFAULT 'user'; scope_companion_id is nullable
        // on memories — normalize NULL/absent to '' so the shared/private query
        // (`scope_kind='user' OR scope_companion_id=?`) and the wire shape are
        // uniform with skills.
        scope_kind: row.try_get::<String, _>("scope_kind").unwrap_or_else(|_| "user".to_string()),
        scope_companion_id: row.try_get::<Option<String>, _>("scope_companion_id").ok().flatten().unwrap_or_default(),
    }
}

fn row_to_companion_thread(row: &sqlx::sqlite::SqliteRow) -> CompanionThread {
    CompanionThread {
        conversation_id: row.get("conversation_id"),
        companion_id: row.get("companion_id"),
        title: row.get("title"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_learn_run(row: &sqlx::sqlite::SqliteRow) -> CompanionLearnRun {
    CompanionLearnRun {
        id: row.get("id"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        status: row.get("status"),
        events_processed: row.get("events_processed"),
        memories_added: row.get("memories_added"),
        suggestions_added: row.get("suggestions_added"),
        error: row.get("error"),
        summary: row.get("summary"),
    }
}

fn row_to_suggestion(row: &sqlx::sqlite::SqliteRow) -> CompanionSuggestion {
    let action: Option<String> = row.get("action");
    CompanionSuggestion {
        id: row.get("id"),
        kind: row.get("kind"),
        title: row.get("title"),
        body: row.get("body"),
        action: action.and_then(|a| serde_json::from_str(&a).ok()),
        status: row.get("status"),
        created_at: row.get("created_at"),
        decided_at: row.get("decided_at"),
    }
}

impl CompanionStore {
    /// Open (or create) `{companion_dir}/memory.db` and apply the schema.
    ///
    /// DDL runs on a dedicated bootstrap pool (one connection) that is closed
    /// before the real pool exists: sqlite connections cache the schema at
    /// statement-prepare time, so if migrations ran on a shared multi-
    /// connection pool, sibling connections opened before an ALTER would keep
    /// serving the pre-ALTER shape (`SELECT *` row materialization can then
    /// panic or silently drop rows). With the bootstrap split, every real
    /// pool connection is born after the final schema.
    pub async fn open(companion_dir: &Path) -> Result<Self, AppError> {
        std::fs::create_dir_all(companion_dir).map_err(|e| AppError::Internal(format!("create companion dir: {e}")))?;
        let opts = SqliteConnectOptions::new()
            .filename(companion_dir.join("memory.db"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        {
            let bootstrap = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts.clone())
                .await
                .map_err(db_err)?;
            let init = init_schema(&bootstrap).await;
            bootstrap.close().await;
            init?;
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(opts)
            .await
            .map_err(db_err)?;
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
        init_schema(&pool).await?;
        Ok(Self { pool })
    }

    // ----- state kv -----

    pub async fn get_state(&self, key: &str) -> Result<Option<String>, AppError> {
        let row = sqlx::query("SELECT value FROM companion_state WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.map(|r| r.get("value")))
    }

    pub async fn set_state(&self, key: &str, value: &str) -> Result<(), AppError> {
        sqlx::query("INSERT INTO companion_state(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn get_state_i64(&self, key: &str) -> Result<i64, AppError> {
        Ok(self
            .get_state(key)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    /// Atomic XP increment (single upsert — concurrent callers never lose a
    /// delta to read-modify-write interleaving). Returns the new total.
    // legacy global xp — only read during boot backfill
    pub async fn add_xp(&self, delta: i64) -> Result<i64, AppError> {
        let row = sqlx::query(
            "INSERT INTO companion_state(key, value) VALUES('xp', ?)
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + ? AS TEXT)
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
        let row = sqlx::query("SELECT value FROM companion_runtime_state WHERE companion_id = ? AND key = ?")
            .bind(companion_id)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.map(|r| r.get("value")))
    }

    pub async fn set_companion_state(&self, companion_id: &str, key: &str, value: &str) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO companion_runtime_state(companion_id, key, value) VALUES(?, ?, ?)
             ON CONFLICT(companion_id, key) DO UPDATE SET value = excluded.value",
        )
        .bind(companion_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn get_companion_state_i64(&self, companion_id: &str, key: &str) -> Result<i64, AppError> {
        Ok(self
            .get_companion_state(companion_id, key)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    /// Atomic per-companion XP increment (single upsert, key fixed to 'xp').
    /// Returns the companion's new total.
    pub async fn add_companion_xp(&self, companion_id: &str, delta: i64) -> Result<i64, AppError> {
        let row = sqlx::query(
            "INSERT INTO companion_runtime_state(companion_id, key, value) VALUES(?, 'xp', ?)
             ON CONFLICT(companion_id, key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + ? AS TEXT)
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
    /// thread registrations) in one transaction. Used by companion deletion.
    pub async fn delete_companion_rows(&self, companion_id: &str) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query("DELETE FROM companion_runtime_state WHERE companion_id = ?")
            .bind(companion_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("DELETE FROM companion_threads WHERE companion_id = ?")
            .bind(companion_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
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
        // Backward-compatible shared insert (the learner hub + legacy callers).
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
        let (scope_kind, scope_companion_id) = scope.columns();
        let mem = CompanionMemory {
            id: generate_prefixed_id("mem"),
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
            "INSERT INTO companion_memories(id, kind, content, tags, importance, strength, pinned, source, status, created_at, updated_at, last_reinforced_at, scope_kind, scope_companion_id)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&mem.id)
        .bind(&mem.kind)
        .bind(&mem.content)
        .bind(serde_json::to_string(&mem.tags).unwrap_or_else(|_| "[]".into()))
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
        let rows = sqlx::query("SELECT id, content FROM companion_memories WHERE kind = ? AND status = 'active'")
            .bind(kind)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        for row in rows {
            let existing: String = row.get("content");
            let existing_norm = existing.trim().to_lowercase();
            if existing_norm == norm {
                return Ok(Some(row.get("id")));
            }
            let (short_len, long_len) = {
                let a = norm.chars().count();
                let b = existing_norm.chars().count();
                (a.min(b), a.max(b))
            };
            let close_in_length = long_len > 0 && (short_len as f64 / long_len as f64) >= CONTAINMENT_MIN_RATIO;
            if close_in_length && (existing_norm.contains(&norm) || norm.contains(&existing_norm)) {
                return Ok(Some(row.get("id")));
            }
        }
        Ok(None)
    }

    pub async fn list_memories(&self, filter: &MemoryFilter) -> Result<Vec<CompanionMemory>, AppError> {
        let mut sql = String::from("SELECT * FROM companion_memories WHERE 1=1");
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
        Ok(rows.iter().map(row_to_memory).collect())
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
        id: &str,
        content: Option<&str>,
        pinned: Option<bool>,
        status: Option<&str>,
        scope: Option<MemoryScope>,
    ) -> Result<(), AppError> {
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
        let (scope_kind, scope_companion_id) = match &scope {
            Some(s) => {
                let (k, c) = s.columns();
                (Some(k.to_owned()), Some(c))
            }
            None => (None, None),
        };
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE companion_memories SET
               content = COALESCE(?, content),
               pinned = COALESCE(?, pinned),
               status = COALESCE(?, status),
               scope_kind = COALESCE(?, scope_kind),
               scope_companion_id = COALESCE(?, scope_companion_id),
               updated_at = ?
             WHERE id = ?",
        )
        .bind(redacted.as_deref())
        .bind(pinned.map(|p| p as i64))
        .bind(status)
        .bind(scope_kind)
        .bind(scope_companion_id)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!("memory '{id}' not found")));
        }
        Ok(())
    }

    pub async fn delete_memory(&self, id: &str) -> Result<(), AppError> {
        sqlx::query("DELETE FROM companion_memories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Reinforce: bump strength toward 1.0 and refresh the reinforcement clock.
    pub async fn reinforce_memories(&self, ids: &[String]) -> Result<(), AppError> {
        let now = now_ms();
        for id in ids {
            sqlx::query(
                "UPDATE companion_memories SET strength = MIN(1.0, strength + 0.2), last_reinforced_at = ?, updated_at = ?, status = 'active' WHERE id = ?",
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
            sqlx::query("UPDATE companion_memories SET status = 'archived', updated_at = ? WHERE id = ?")
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
            "SELECT id, kind, strength, last_reinforced_at FROM companion_memories WHERE status = 'active' AND pinned = 0",
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
            let id: String = row.get("id");
            if decayed < ARCHIVE_THRESHOLD {
                sqlx::query("UPDATE companion_memories SET strength = ?, status = 'archived', updated_at = ? WHERE id = ?")
                    .bind(decayed)
                    .bind(now)
                    .bind(&id)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
                archived += 1;
            } else {
                sqlx::query("UPDATE companion_memories SET strength = ? WHERE id = ?")
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
        let mut picked: Vec<CompanionMemory> = Vec::new();
        let pinned = sqlx::query(
            "SELECT * FROM companion_memories WHERE status = 'active' AND pinned = 1 AND (scope_kind = 'user' OR scope_companion_id = ?) ORDER BY strength DESC",
        )
        .bind(companion_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        picked.extend(pinned.iter().map(row_to_memory));
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
            picked.extend(rows.iter().map(row_to_memory));
        }
        let mut used = 0usize;
        picked.retain(|m| {
            used += m.content.len();
            used <= char_budget
        });
        Ok(picked)
    }

    // ----- suggestions -----

    pub async fn insert_suggestion(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        action: Option<&serde_json::Value>,
    ) -> Result<CompanionSuggestion, AppError> {
        let now = now_ms();
        let s = CompanionSuggestion {
            id: generate_prefixed_id("sug"),
            kind: kind.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
            action: action.cloned(),
            status: "new".into(),
            created_at: now,
            decided_at: None,
        };
        sqlx::query("INSERT INTO companion_suggestions(id, kind, title, body, action, status, created_at) VALUES(?,?,?,?,?,?,?)")
            .bind(&s.id)
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
        let rows = sqlx::query("SELECT id, title, body FROM companion_suggestions WHERE kind = ? AND status = 'new'")
            .bind(kind)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        for row in rows {
            let existing_title: String = row.get("title");
            let existing_title = existing_title.trim().to_lowercase();
            if !norm_title.is_empty() && existing_title == norm_title {
                return Ok(Some(row.get("id")));
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
                return Ok(Some(row.get("id")));
            }
            if !norm_body.is_empty() {
                let existing_body: String = row.get("body");
                if existing_body.trim().to_lowercase() == norm_body {
                    return Ok(Some(row.get("id")));
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
    pub async fn touch_suggestion(&self, id: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE companion_suggestions SET created_at = ? WHERE id = ? AND status = 'new'")
            .bind(now_ms())
            .bind(id)
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
        Ok(rows.iter().map(row_to_suggestion).collect())
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
    pub async fn decide_suggestion(&self, id: &str, accept: bool) -> Result<(CompanionSuggestion, bool), AppError> {
        let status = if accept { "accepted" } else { "dismissed" };
        let result = sqlx::query("UPDATE companion_suggestions SET status = ?, decided_at = ? WHERE id = ? AND status = 'new'")
            .bind(status)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        let newly_decided = result.rows_affected() >= 1;
        // Always read back: rows_affected == 0 means either the row is gone
        // (true 404) or it was already decided (idempotent success).
        let row = sqlx::query("SELECT * FROM companion_suggestions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        match row {
            Some(row) => Ok((row_to_suggestion(&row), newly_decided)),
            None => Err(AppError::NotFound(format!("suggestion '{id}' not found"))),
        }
    }

    // ----- learn runs -----

    pub async fn insert_learn_run(&self, run: &CompanionLearnRun) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO companion_learn_runs(id, started_at, finished_at, status, events_processed, memories_added, suggestions_added, error, summary)
             VALUES(?,?,?,?,?,?,?,?,?)",
        )
        .bind(&run.id)
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
        Ok(rows
            .iter()
            .map(|row| CompanionLearnRun {
                id: row.get("id"),
                started_at: row.get("started_at"),
                finished_at: row.get("finished_at"),
                status: row.get("status"),
                events_processed: row.get("events_processed"),
                memories_added: row.get("memories_added"),
                suggestions_added: row.get("suggestions_added"),
                error: row.get("error"),
                summary: row.get("summary"),
            })
            .collect())
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
            let rows = sqlx::query("SELECT * FROM companion_memories WHERE id > ? ORDER BY id LIMIT ?")
                .bind(&cursor)
                .bind(Self::DUMP_PAGE)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
            let Some(last) = rows.last() else { break };
            cursor = last.get("id");
            out.extend(rows.iter().map(row_to_memory));
        }
        Ok(out)
    }

    /// Every `companion_learn_runs` row via the same id cursor as
    /// [`dump_memories_all`]. Ordered by id.
    pub async fn dump_learn_runs_all(&self) -> Result<Vec<CompanionLearnRun>, AppError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let rows = sqlx::query("SELECT * FROM companion_learn_runs WHERE id > ? ORDER BY id LIMIT ?")
                .bind(&cursor)
                .bind(Self::DUMP_PAGE)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
            let Some(last) = rows.last() else { break };
            cursor = last.get("id");
            out.extend(rows.iter().map(row_to_learn_run));
        }
        Ok(out)
    }

    pub async fn get_memory(&self, id: &str) -> Result<Option<CompanionMemory>, AppError> {
        let row = sqlx::query("SELECT * FROM companion_memories WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.as_ref().map(row_to_memory))
    }

    /// Fidelity insert for import: every field (id, timestamps, strength,
    /// pinned, source, status, …) is written exactly as given — unlike
    /// [`insert_memory`], nothing is regenerated or clamped. The caller is
    /// responsible for id-collision handling (see `export::import_bundle`).
    pub async fn insert_memory_raw(&self, mem: &CompanionMemory) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO companion_memories(id, kind, content, tags, importance, strength, pinned, source, status, created_at, updated_at, last_reinforced_at, scope_kind, scope_companion_id)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&mem.id)
        .bind(&mem.kind)
        .bind(&mem.content)
        .bind(serde_json::to_string(&mem.tags).unwrap_or_else(|_| "[]".into()))
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
        let row = sqlx::query("SELECT 1 AS x FROM companion_learn_runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    // ----- companion threads -----

    /// Register a conversation as a companion thread (idempotent upsert).
    /// The owning companion is fixed at registration: re-registering an existing
    /// thread refreshes title/clock but never re-homes it to another companion.
    /// That also means legacy rows with an empty companion_id cannot be claimed
    /// through this upsert — the T1.4 backfill must use a dedicated UPDATE
    /// targeting `companion_id = ''` instead.
    pub async fn insert_companion_thread(
        &self,
        conversation_id: &str,
        companion_id: &str,
        title: &str,
    ) -> Result<CompanionThread, AppError> {
        let now = now_ms();
        // `companion_id = excluded.companion_id`: rebind a reused conversation_id to the
        // companion that just created it. conversation ids are now INTEGER (not
        // globally-unique strings), so a colliding id means the prior binding
        // is stale (its conversation no longer exists) — `companion.create`
        // only ever inserts a freshly-minted conversation, so the latest caller
        // is the rightful owner. See numeric-id spec §2.5 (reused-id staleness).
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
        Ok(row_to_companion_thread(&row))
    }

    /// Threads, most recently touched first — all of them, or only one companion's.
    pub async fn list_companion_threads(&self, companion_id: Option<&str>) -> Result<Vec<CompanionThread>, AppError> {
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
        Ok(rows.iter().map(row_to_companion_thread).collect())
    }

    /// The owning companion of a registered thread. Unregistered conversations and
    /// legacy rows with an empty (not yet backfilled) companion_id are both `None`.
    pub async fn thread_companion_id(&self, conversation_id: &str) -> Result<Option<String>, AppError> {
        let row = sqlx::query("SELECT companion_id FROM companion_threads WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row
            .map(|r| r.get::<String, _>("companion_id"))
            .filter(|id| !id.is_empty()))
    }

    pub async fn is_companion_thread(&self, conversation_id: &str) -> Result<bool, AppError> {
        let row = sqlx::query("SELECT 1 AS x FROM companion_threads WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    /// Rename and/or bump the activity clock of a thread.
    pub async fn touch_companion_thread(&self, conversation_id: &str, title: Option<&str>) -> Result<(), AppError> {
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
        sqlx::query("DELETE FROM companion_threads WHERE conversation_id = ?")
            .bind(conversation_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    // ----- legacy backfill -----

    /// Post-migration backfill (idempotent): stamp the single legacy companion
    /// row and move legacy global xp / companion_active_thread into
    /// companion_runtime_state.
    ///
    /// Single-session invariant (work-partner refactor): a companion owns at most one
    /// companion thread, enforced by the v2 UNIQUE INDEX. So this claims at
    /// most ONE empty-companion_id legacy row for `companion_id` — the most recently
    /// updated — and drops every other empty-companion_id row (registry rows only;
    /// conversations + companion_memories untouched). It also skips the claim
    /// entirely when `companion_id` already owns a thread (re-run safety), since a
    /// blanket re-home would collide with the unique index.
    ///
    /// The companion-thread upsert deliberately never re-homes existing rows,
    /// so claiming the empty-companion_id legacy row needs this dedicated UPDATE.
    /// The global kv keys are only moved when the per-companion slot is still empty
    /// (a value earned post-migration must win), then deleted either way so a
    /// second run finds nothing left to move.
    pub async fn backfill_first_companion(&self, companion_id: &str) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // The legacy row to keep: newest empty-companion_id thread (ties → larger
        // conversation_id). NULL when there are no legacy rows.
        let keep: Option<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM companion_threads
             WHERE companion_id = ''
             ORDER BY updated_at DESC, conversation_id DESC
             LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        // Drop the surplus legacy rows regardless (single-session invariant);
        // the conversations they pointed at are not touched here.
        sqlx::query("DELETE FROM companion_threads WHERE companion_id = '' AND conversation_id != COALESCE(?, '')")
            .bind(&keep)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        // Claim the kept row only when the companion has no thread yet — re-homing
        // onto a companion that already owns one would hit the unique index.
        let already_owns: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM companion_threads WHERE companion_id = ?")
                .bind(companion_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
        if already_owns == 0 {
            sqlx::query("UPDATE companion_threads SET companion_id = ? WHERE companion_id = ''")
                .bind(companion_id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        } else {
            // Companion already has a thread: discard the leftover legacy row too.
            sqlx::query("DELETE FROM companion_threads WHERE companion_id = ''")
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        for key in ["xp", "companion_active_thread"] {
            if let Some(value) = self.get_state(key).await? {
                if self.get_companion_state(companion_id, key).await?.is_none() {
                    self.set_companion_state(companion_id, key, &value).await?;
                }
            }
        }
        sqlx::query("DELETE FROM companion_state WHERE key IN ('xp','companion_active_thread')")
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 自进化：技能注册表 / 挖矿统计 / 反馈回流
// 正文以磁盘 SKILL.md 为事实源（见 nomifun-extension::skill_service）；这里只存
// 元数据 + 溯源 + 生命周期。scope_companion_id == "" 表示 shared（全员可用）。
// ---------------------------------------------------------------------------

/// 一个伙伴自进化技能的注册表行。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompanionSkill {
    pub skill_name: String,
    pub scope_kind: String,
    /// "" = shared（全员可用）；否则为拥有该专属技能的 companion_id。
    pub scope_companion_id: String,
    pub status: String,
    pub source: String,
    pub confidence: f64,
    pub provenance: Vec<String>,
    pub strength: f64,
    pub version: i64,
    pub superseded_by: Option<String>,
    pub usage_count: i64,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Originating mined-pattern signature ("" for manual/demonstrated skills);
    /// used to suppress a rejected pattern from re-proposal (纠偏回流).
    #[serde(default)]
    pub signature: String,
}

fn row_to_skill(row: &sqlx::sqlite::SqliteRow) -> CompanionSkill {
    let prov: String = row.get("provenance");
    CompanionSkill {
        skill_name: row.get("skill_name"),
        scope_kind: row.get("scope_kind"),
        scope_companion_id: row.get("scope_companion_id"),
        status: row.get("status"),
        source: row.get("source"),
        confidence: row.get("confidence"),
        provenance: serde_json::from_str(&prov).unwrap_or_default(),
        strength: row.get("strength"),
        version: row.get("version"),
        superseded_by: row.get("superseded_by"),
        usage_count: row.get("usage_count"),
        last_used_at: row.get("last_used_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        signature: row.get("signature"),
    }
}

impl CompanionStore {
    /// UPSERT a skill registry row (keyed by scope + name).
    pub async fn insert_skill(&self, s: &CompanionSkill) -> Result<(), AppError> {
        let prov = serde_json::to_string(&s.provenance).unwrap_or_else(|_| "[]".into());
        sqlx::query(
            "INSERT INTO companion_skills(skill_name, scope_kind, scope_companion_id, status, source, confidence,
                provenance, strength, version, superseded_by, usage_count, last_used_at, created_at, updated_at, signature)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(scope_kind, scope_companion_id, skill_name) DO UPDATE SET
                status=excluded.status, source=excluded.source, confidence=excluded.confidence,
                provenance=excluded.provenance, strength=excluded.strength, version=excluded.version,
                superseded_by=excluded.superseded_by, updated_at=excluded.updated_at, signature=excluded.signature",
        )
        .bind(&s.skill_name)
        .bind(&s.scope_kind)
        .bind(&s.scope_companion_id)
        .bind(&s.status)
        .bind(&s.source)
        .bind(s.confidence)
        .bind(&prov)
        .bind(s.strength)
        .bind(s.version)
        .bind(&s.superseded_by)
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
        let sql = if include_shared {
            "SELECT * FROM companion_skills WHERE scope_companion_id = ? OR scope_kind = 'user' ORDER BY strength DESC"
        } else {
            "SELECT * FROM companion_skills WHERE scope_companion_id = ? ORDER BY strength DESC"
        };
        let rows = sqlx::query(sql).bind(companion_id).fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().map(row_to_skill).collect())
    }

    pub async fn get_skill(&self, companion_id: &str, name: &str) -> Result<Option<CompanionSkill>, AppError> {
        let row = sqlx::query("SELECT * FROM companion_skills WHERE scope_companion_id = ? AND skill_name = ?")
            .bind(companion_id)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.as_ref().map(row_to_skill))
    }

    pub async fn set_skill_status(&self, companion_id: &str, name: &str, status: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE companion_skills SET status = ? WHERE scope_companion_id = ? AND skill_name = ?")
            .bind(status)
            .bind(companion_id)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn record_skill_usage(&self, companion_id: &str, name: &str, now: i64) -> Result<(), AppError> {
        // Bump usage AND reinforce strength toward 1.0 (mirrors reinforce_memories) so that
        // a frequently-used skill survives the decay pass — "used skills stay sharp".
        sqlx::query(
            "UPDATE companion_skills SET usage_count = usage_count + 1, last_used_at = ?, \
             strength = MIN(1.0, strength + 0.1), updated_at = ? WHERE scope_companion_id = ? AND skill_name = ?",
        )
        .bind(now)
        .bind(now)
        .bind(companion_id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Decay active-skill strength by age since last use; auto-archive those below threshold.
    /// Manual/demonstrated skills (`source != 'mined'`) never decay (analog of profile memories).
    /// This is NOT a user rejection: it writes no feedback and never suppresses the originating
    /// pattern, so resumed behavior can be re-mined. Only flips the DB row (SKILL.md stays). Returns archived count.
    pub async fn decay_skills(&self, half_life_days: f64, archive_threshold: f64) -> Result<i64, AppError> {
        let now = now_ms();
        let rows = sqlx::query(
            "SELECT scope_companion_id, skill_name, source, strength, COALESCE(last_used_at, created_at) AS clock \
             FROM companion_skills WHERE status = 'active'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let half = half_life_days.max(0.1);
        let mut archived = 0i64;
        for row in rows {
            let source: String = row.get("source");
            if source != "mined" {
                continue; // manual / demonstrated / gifted skills never decay
            }
            let strength: f64 = row.get("strength");
            let clock: i64 = row.get("clock");
            let age_days = ((now - clock).max(0)) as f64 / 86_400_000.0;
            let decayed = strength * 0.5f64.powf(age_days / half);
            let cid: String = row.get("scope_companion_id");
            let name: String = row.get("skill_name");
            if decayed < archive_threshold {
                sqlx::query("UPDATE companion_skills SET strength = ?, status = 'archived', updated_at = ? WHERE scope_companion_id = ? AND skill_name = ?")
                    .bind(decayed)
                    .bind(now)
                    .bind(&cid)
                    .bind(&name)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
                archived += 1;
            } else {
                sqlx::query("UPDATE companion_skills SET strength = ? WHERE scope_companion_id = ? AND skill_name = ?")
                    .bind(decayed)
                    .bind(&cid)
                    .bind(&name)
                    .execute(&self.pool)
                    .await
                    .map_err(db_err)?;
            }
        }
        Ok(archived)
    }

    /// Count a companion's own active skills (for the expertise badge).
    pub async fn count_active_skills(&self, companion_id: &str) -> Result<i64, AppError> {
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
    /// Returns the existing skill_name. Same-name is excluded (the insert UPSERT handles that).
    pub async fn find_similar_skill(&self, companion_id: &str, name: &str) -> Result<Option<String>, AppError> {
        let target = name.to_lowercase();
        let rows = sqlx::query(
            "SELECT skill_name FROM companion_skills WHERE scope_companion_id = ? AND status IN ('active','draft')",
        )
        .bind(companion_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        for row in rows {
            let existing: String = row.get("skill_name");
            if existing == name {
                continue; // same name → UPSERT path, not a "similar" hit
            }
            let e = existing.to_lowercase();
            if e == target {
                return Ok(Some(existing));
            }
            let (short, long) = if e.len() <= target.len() { (&e, &target) } else { (&target, &e) };
            if !short.is_empty() && long.contains(short.as_str()) && (short.len() as f64 / long.len() as f64) >= 0.6 {
                return Ok(Some(existing));
            }
        }
        Ok(None)
    }

    /// Bump a skill's version (on evolve-in-place).
    pub async fn bump_skill_version(&self, companion_id: &str, name: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE companion_skills SET version = version + 1, updated_at = ? WHERE scope_companion_id = ? AND skill_name = ?")
            .bind(now_ms())
            .bind(companion_id)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// 记录一次模式出现：累加 count，并把 `session_id::event_id` 收进样本集；
    /// distinct_sessions = 样本集去重 session 数。返回当前 distinct_sessions。
    pub async fn bump_pattern(&self, signature: &str, session_id: &str, event_id: &str, now: i64) -> Result<i64, AppError> {
        let existing: Option<String> = sqlx::query_scalar("SELECT example_event_ids FROM skill_pattern_stats WHERE signature = ?")
            .bind(signature)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        let mut ids: Vec<String> = existing.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
        ids.push(format!("{session_id}::{event_id}"));
        if ids.len() > 50 {
            let cut = ids.len() - 50;
            ids.drain(0..cut);
        }
        let distinct: std::collections::HashSet<&str> = ids.iter().filter_map(|x| x.split("::").next()).collect();
        let distinct_n = distinct.len() as i64;
        let ids_json = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into());
        sqlx::query(
            "INSERT INTO skill_pattern_stats(signature, count, distinct_sessions, example_event_ids, status, last_seen)
             VALUES(?, 1, ?, ?, 'open', ?)
             ON CONFLICT(signature) DO UPDATE SET count = count + 1, distinct_sessions = ?, example_event_ids = ?, last_seen = ?",
        )
        .bind(signature)
        .bind(distinct_n)
        .bind(&ids_json)
        .bind(now)
        .bind(distinct_n)
        .bind(&ids_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(distinct_n)
    }

    pub async fn mark_pattern_status(&self, signature: &str, status: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE skill_pattern_stats SET status = ? WHERE signature = ?")
            .bind(status)
            .bind(signature)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Current status of a mined pattern signature (`open`/`drafted`/`rejected`), or `None` if unseen.
    pub async fn pattern_status(&self, signature: &str) -> Result<Option<String>, AppError> {
        let row = sqlx::query_scalar::<_, String>("SELECT status FROM skill_pattern_stats WHERE signature = ?")
            .bind(signature)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row)
    }

    pub async fn record_feedback(
        &self,
        id: &str,
        draft_id: &str,
        signature: Option<&str>,
        decision: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<(), AppError> {
        sqlx::query("INSERT INTO evolution_feedback(id, draft_id, signature, decision, reason, created_at) VALUES(?,?,?,?,?,?)")
            .bind(id)
            .bind(draft_id)
            .bind(signature)
            .bind(decision)
            .bind(reason)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// 是否曾被拒绝（负样本）：存在 decision='reject' 的反馈即视为该签名被否决。
    pub async fn is_signature_rejected(&self, signature: &str) -> Result<bool, AppError> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM evolution_feedback WHERE signature = ? AND decision = 'reject'")
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

    #[tokio::test]
    async fn pattern_bump_and_reject_feedback() {
        let store = CompanionStore::open_memory().await.unwrap();
        // 同 signature、不同 session → distinct_sessions 递增
        assert_eq!(store.bump_pattern("sig-A", "s1", "e1", 10).await.unwrap(), 1);
        assert_eq!(store.bump_pattern("sig-A", "s1", "e2", 11).await.unwrap(), 1); // 同 session 不增 distinct
        assert_eq!(store.bump_pattern("sig-A", "s2", "e3", 12).await.unwrap(), 2);
        // 反馈：拒绝 → 负样本
        assert!(!store.is_signature_rejected("sig-A").await.unwrap());
        store.record_feedback("f1", "draft-1", Some("sig-A"), "reject", Some("too narrow"), 13).await.unwrap();
        assert!(store.is_signature_rejected("sig-A").await.unwrap());
    }

    #[tokio::test]
    async fn insert_list_get_skill_roundtrip() {
        let store = CompanionStore::open_memory().await.unwrap();
        let s = CompanionSkill {
            skill_name: "weekly-report".into(),
            scope_kind: "companion".into(),
            scope_companion_id: "c1".into(),
            status: "draft".into(),
            source: "mined".into(),
            confidence: 0.7,
            provenance: vec!["e1".into(), "e2".into()],
            strength: 1.0,
            version: 1,
            superseded_by: None,
            usage_count: 0,
            last_used_at: None,
            created_at: 100,
            updated_at: 100,
            signature: String::new(),
        };
        store.insert_skill(&s).await.unwrap();
        let got = store.get_skill("c1", "weekly-report").await.unwrap().unwrap();
        assert_eq!(got.confidence, 0.7);
        assert_eq!(got.provenance, vec!["e1".to_string(), "e2".to_string()]);
        let listed = store.list_skills("c1", false).await.unwrap();
        assert_eq!(listed.len(), 1);
        store.set_skill_status("c1", "weekly-report", "active").await.unwrap();
        assert_eq!(store.get_skill("c1", "weekly-report").await.unwrap().unwrap().status, "active");
        store.record_skill_usage("c1", "weekly-report", 200).await.unwrap();
        let after = store.get_skill("c1", "weekly-report").await.unwrap().unwrap();
        assert_eq!(after.usage_count, 1);
        assert_eq!(after.last_used_at, Some(200));
    }

    #[tokio::test]
    async fn fresh_db_has_evolution_tables() {
        let store = CompanionStore::open_memory().await.unwrap();
        for t in ["companion_skills", "skill_pattern_stats", "evolution_feedback"] {
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?")
                .bind(t)
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(n, 1, "missing table {t}");
        }
    }

    #[tokio::test]
    async fn fresh_db_companion_skills_has_signature_column() {
        let store = CompanionStore::open_memory().await.unwrap();
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(companion_skills)")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.contains(&"signature".to_string()), "companion_skills missing signature column");
    }

    #[tokio::test]
    async fn decay_archives_unused_mined_skill_but_spares_manual() {
        let store = CompanionStore::open_memory().await.unwrap();
        let old = now_ms() - 365 * 86_400_000; // ~1 year ago, never used
        let mk = |name: &str, source: &str| CompanionSkill {
            skill_name: name.into(),
            scope_kind: "companion".into(),
            scope_companion_id: "c1".into(),
            status: "active".into(),
            source: source.into(),
            confidence: 0.7,
            provenance: vec![],
            strength: 1.0,
            version: 1,
            superseded_by: None,
            usage_count: 0,
            last_used_at: None,
            created_at: old,
            updated_at: old,
            signature: "sig".into(),
        };
        store.insert_skill(&mk("stale-mined", "mined")).await.unwrap();
        store.insert_skill(&mk("manual-skill", "manual")).await.unwrap();
        let archived = store.decay_skills(45.0, 0.05).await.unwrap();
        assert_eq!(archived, 1, "only the stale mined skill should archive");
        assert_eq!(store.get_skill("c1", "stale-mined").await.unwrap().unwrap().status, "archived");
        assert_eq!(store.get_skill("c1", "manual-skill").await.unwrap().unwrap().status, "active", "manual skills never decay");
        assert_eq!(store.count_active_skills("c1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn existing_v2_db_gains_evolution_tables_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        // 造一个最小 v2 库
        {
            let p = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(SqliteConnectOptions::new().filename(&path).create_if_missing(true))
                .await
                .unwrap();
            sqlx::raw_sql(
                "CREATE TABLE companion_threads (conversation_id TEXT PRIMARY KEY, companion_id TEXT NOT NULL DEFAULT '', title TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0); PRAGMA user_version = 2;",
            )
            .execute(&p)
            .await
            .unwrap();
            p.close().await;
        }
        let store = CompanionStore::open(dir.path()).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='companion_skills'")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn fresh_db_born_at_v3_with_scope_columns() {
        let store = CompanionStore::open_memory().await.unwrap();
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(companion_memories)")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.contains(&"scope_kind".to_string()), "missing scope_kind");
        assert!(cols.contains(&"scope_companion_id".to_string()), "missing scope_companion_id");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&store.pool).await.unwrap();
        assert_eq!(version, STORE_VERSION);
    }

    #[tokio::test]
    async fn migrate_v2_to_v3_adds_scope_columns_idempotent() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap();
        // 造一个 v2 库：companion_memories 无 scope 列，user_version=2
        sqlx::raw_sql(
            "CREATE TABLE companion_memories (id TEXT PRIMARY KEY, kind TEXT NOT NULL, content TEXT NOT NULL,
               tags TEXT NOT NULL DEFAULT '[]', importance REAL NOT NULL DEFAULT 0.5, strength REAL NOT NULL DEFAULT 0.5,
               pinned INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL DEFAULT 'learn', status TEXT NOT NULL DEFAULT 'active',
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, last_reinforced_at INTEGER NOT NULL);
             PRAGMA user_version = 2;",
        )
        .execute(&pool)
        .await
        .unwrap();
        apply_migrations(&pool).await.unwrap();
        apply_migrations(&pool).await.unwrap(); // 二次应跑无错
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(companion_memories)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.contains(&"scope_kind".to_string()));
        assert!(cols.contains(&"scope_companion_id".to_string()));
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(version, STORE_VERSION);
    }

    #[tokio::test]
    async fn memory_crud_reinforce_archive() {
        let store = CompanionStore::open_memory().await.unwrap();
        let m = store
            .insert_memory("preference", "主人喜欢简洁的中文回复", &["风格".into()], 0.8, "learn")
            .await
            .unwrap();
        assert_eq!(store.count_memories("active").await.unwrap(), 1);

        store.reinforce_memories(&[m.id.clone()]).await.unwrap();
        let listed = store.list_memories(&MemoryFilter::default()).await.unwrap();
        assert!((listed[0].strength - 1.0).abs() < 1e-9);

        store.archive_memories(&[m.id.clone()]).await.unwrap();
        assert_eq!(store.count_memories("active").await.unwrap(), 0);
        assert_eq!(store.count_memories("archived").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn insert_memory_redacts_secret_in_content() {
        let store = CompanionStore::open_memory().await.unwrap();
        let mem = store
            .insert_memory(
                "knowledge",
                "我的 key 是 sk-ABCDEFGHIJ0123456789xyz 别外泄",
                &[],
                0.8,
                "chat",
            )
            .await
            .unwrap();
        assert!(mem.content.contains("[REDACTED_SECRET]"), "got: {}", mem.content);
        assert!(!mem.content.contains("sk-ABCDEFGHIJ"));
    }

    #[tokio::test]
    async fn private_memory_injects_only_for_owner() {
        let store = CompanionStore::open_memory().await.unwrap();
        store.insert_memory("knowledge", "shared fact", &[], 0.9, "learn").await.unwrap();
        store
            .insert_memory_scoped("preference", "c1 likes tea", &[], 0.9, "chat", MemoryScope::Companion("c1".into()))
            .await
            .unwrap();
        store
            .insert_memory_scoped("preference", "c2 likes coffee", &[], 0.9, "chat", MemoryScope::Companion("c2".into()))
            .await
            .unwrap();

        let c1 = store.memories_for_injection("c1", 10, 100_000).await.unwrap();
        let texts: Vec<&str> = c1.iter().map(|m| m.content.as_str()).collect();
        assert!(texts.contains(&"shared fact"), "owner sees shared: {texts:?}");
        assert!(texts.contains(&"c1 likes tea"), "owner sees own private: {texts:?}");
        assert!(!texts.contains(&"c2 likes coffee"), "owner must NOT see another companion's private: {texts:?}");
    }

    #[tokio::test]
    async fn list_memories_scope_filter_excludes_other_private() {
        let store = CompanionStore::open_memory().await.unwrap();
        store.insert_memory("knowledge", "shared fact", &[], 0.9, "learn").await.unwrap();
        store
            .insert_memory_scoped("knowledge", "c1 private", &[], 0.9, "chat", MemoryScope::Companion("c1".into()))
            .await
            .unwrap();
        store
            .insert_memory_scoped("knowledge", "c2 private", &[], 0.9, "chat", MemoryScope::Companion("c2".into()))
            .await
            .unwrap();
        let filter = MemoryFilter { scope_companion_id: Some("c1".into()), ..Default::default() };
        let listed = store.list_memories(&filter).await.unwrap();
        let texts: Vec<&str> = listed.iter().map(|m| m.content.as_str()).collect();
        assert!(texts.contains(&"shared fact"));
        assert!(texts.contains(&"c1 private"));
        assert!(!texts.contains(&"c2 private"));
        // No scope filter → cross-companion "all" view sees everything.
        let all = store.list_memories(&MemoryFilter::default()).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn update_memory_redacts_secret_and_rejects_empty() {
        let store = CompanionStore::open_memory().await.unwrap();
        let m = store.insert_memory("knowledge", "original", &[], 0.8, "manual").await.unwrap();
        store
            .update_memory(&m.id, Some("新 key sk-ABCDEFGHIJ0123456789xyz 收好"), None, None, None)
            .await
            .unwrap();
        let got = store.get_memory(&m.id).await.unwrap().unwrap();
        assert!(got.content.contains("[REDACTED_SECRET]"), "edited content must be redacted: {}", got.content);
        assert!(!got.content.contains("sk-ABCDEFGHIJ"));
        // Empty/whitespace edits are rejected (parity with insert).
        assert!(store.update_memory(&m.id, Some("   "), None, None, None).await.is_err());
    }

    #[tokio::test]
    async fn update_memory_can_change_scope_shared_to_private() {
        let store = CompanionStore::open_memory().await.unwrap();
        let m = store.insert_memory("preference", "fact", &[], 0.8, "manual").await.unwrap();
        assert_eq!(m.scope_kind, "user");
        assert_eq!(m.scope_companion_id, "");
        store
            .update_memory(&m.id, None, None, None, Some(MemoryScope::Companion("c1".into())))
            .await
            .unwrap();
        let got = store.get_memory(&m.id).await.unwrap().unwrap();
        assert_eq!(got.scope_kind, "companion");
        assert_eq!(got.scope_companion_id, "c1");
        // And back to shared.
        store.update_memory(&m.id, None, None, None, Some(MemoryScope::Shared)).await.unwrap();
        let back = store.get_memory(&m.id).await.unwrap().unwrap();
        assert_eq!(back.scope_kind, "user");
        assert_eq!(back.scope_companion_id, "");
    }

    #[tokio::test]
    async fn decay_archives_stale_episodes() {
        let store = CompanionStore::open_memory().await.unwrap();
        let m = store
            .insert_memory("episode", "昨天上线了 X", &[], 0.5, "learn")
            .await
            .unwrap();
        // Backdate the reinforcement clock by 100 days (>> 7d half-life).
        sqlx::query("UPDATE companion_memories SET last_reinforced_at = ? WHERE id = ?")
            .bind(now_ms() - 100 * 86_400_000)
            .bind(&m.id)
            .execute(&store.pool)
            .await
            .unwrap();
        let archived = store.decay_memories().await.unwrap();
        assert_eq!(archived, 1);
        // Pinned profile memories never decay.
        let p = store.insert_memory("profile", "主人是 Rust 工程师", &[], 0.9, "learn").await.unwrap();
        sqlx::query("UPDATE companion_memories SET last_reinforced_at = ? WHERE id = ?")
            .bind(now_ms() - 1000 * 86_400_000)
            .bind(&p.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.decay_memories().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn dedup_finds_similar() {
        let store = CompanionStore::open_memory().await.unwrap();
        store
            .insert_memory("knowledge", "cargo check --workspace 是 Rust 侧的构建门禁", &[], 0.6, "learn")
            .await
            .unwrap();
        let hit = store
            .find_similar_active("knowledge", "cargo check --workspace 是 rust 侧的构建门禁")
            .await
            .unwrap();
        assert!(hit.is_some());
        let miss = store.find_similar_active("knowledge", "完全不同的内容").await.unwrap();
        assert!(miss.is_none());
    }

    #[tokio::test]
    async fn suggestion_decide_and_xp_roundtrip() {
        let store = CompanionStore::open_memory().await.unwrap();
        let s = store
            .insert_suggestion("guess_question", "猜你想问", "要不要看看…", None)
            .await
            .unwrap();
        let (decided, newly) = store.decide_suggestion(&s.id, true).await.unwrap();
        assert_eq!(decided.status, "accepted");
        assert!(newly, "first decide performs the new->accepted transition");
        // Idempotent: deciding again is a no-op that returns the current state
        // (first decision wins), NOT an error — stale cards / double-clicks /
        // cross-surface repeats must not 404.
        let (again, newly_again) = store.decide_suggestion(&s.id, false).await.unwrap();
        assert_eq!(again.status, "accepted", "first decision wins; status unchanged");
        assert!(!newly_again, "no second transition");
        // A genuinely missing row is still NotFound.
        assert!(matches!(
            store.decide_suggestion("sug_doesnotexist000", true).await,
            Err(AppError::NotFound(_))
        ));

        let xp = store.add_xp(5).await.unwrap();
        assert_eq!(xp, 5);
        assert_eq!(store.get_state_i64("xp").await.unwrap(), 5);
    }

    #[tokio::test]
    async fn find_similar_suggestion_matches_pending_only() {
        let store = CompanionStore::open_memory().await.unwrap();
        let s = store
            .insert_suggestion("create_cron", "建议加个每日备份定时任务", "每天 22:00 备份工作目录", None)
            .await
            .unwrap();

        // Same kind + same title (case/space-insensitive) → hit.
        let hit = store
            .find_similar_suggestion("create_cron", " 建议加个每日备份定时任务 ", "随便什么正文")
            .await
            .unwrap();
        assert_eq!(hit.as_deref(), Some(s.id.as_str()));
        // Containment with close lengths → hit.
        let contained = store
            .find_similar_suggestion("create_cron", "加个每日备份定时任务", "")
            .await
            .unwrap();
        assert!(contained.is_some());
        // Same body, different title → hit.
        let body_hit = store
            .find_similar_suggestion("create_cron", "完全不同的标题啊", "每天 22:00 备份工作目录")
            .await
            .unwrap();
        assert!(body_hit.is_some());
        // Different kind → miss.
        assert!(
            store
                .find_similar_suggestion("insight", "建议加个每日备份定时任务", "")
                .await
                .unwrap()
                .is_none()
        );
        // Genuinely different content → miss.
        assert!(
            store
                .find_similar_suggestion("create_cron", "建议固化成技能", "把常用流程沉淀下来")
                .await
                .unwrap()
                .is_none()
        );
        // Decided suggestions no longer block re-raising.
        store.decide_suggestion(&s.id, false).await.unwrap();
        assert!(
            store
                .find_similar_suggestion("create_cron", "建议加个每日备份定时任务", "每天 22:00 备份工作目录")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn touch_suggestion_bumps_pending_created_at_only() {
        let store = CompanionStore::open_memory().await.unwrap();
        let s = store
            .insert_suggestion("insight", "最近常调编译错误", "建议看看构建脚本", None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        store.touch_suggestion(&s.id).await.unwrap();
        let touched = &store.list_suggestions(Some("new"), 10).await.unwrap()[0];
        assert_eq!(touched.id, s.id);
        assert!(touched.created_at > s.created_at, "touch must bump created_at");

        // Decided suggestions are immutable to touch.
        let (decided, _) = store.decide_suggestion(&s.id, true).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        store.touch_suggestion(&s.id).await.unwrap();
        let after = &store.list_suggestions(None, 10).await.unwrap()[0];
        assert_eq!(after.created_at, decided.created_at);
        assert_eq!(after.status, "accepted");
    }

    #[tokio::test]
    async fn companion_thread_crud_roundtrip() {
        let store = CompanionStore::open_memory().await.unwrap();
        assert!(store.list_companion_threads(None).await.unwrap().is_empty());
        assert!(!store.is_companion_thread("conv_a").await.unwrap());

        store.insert_companion_thread("conv_a", "companion_1", "第一段对话").await.unwrap();
        store.insert_companion_thread("conv_b", "companion_2", "第二段").await.unwrap();
        assert!(store.is_companion_thread("conv_a").await.unwrap());

        // touch bumps the activity clock so conv_a sorts first.
        store.touch_companion_thread("conv_a", Some("改名了")).await.unwrap();
        let threads = store.list_companion_threads(None).await.unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].conversation_id, "conv_a");
        assert_eq!(threads[0].title, "改名了");
        assert_eq!(threads[0].companion_id, "companion_1");

        // Per-companion filter only sees that companion's threads.
        let companion2_threads = store.list_companion_threads(Some("companion_2")).await.unwrap();
        assert_eq!(companion2_threads.len(), 1);
        assert_eq!(companion2_threads[0].conversation_id, "conv_b");

        // Re-inserting a conversation_id rebinds it to the new companion (reused
        // INTEGER ids ⇒ the prior binding was stale). Title/clock update too.
        let again = store.insert_companion_thread("conv_a", "companion_9", "再次注册").await.unwrap();
        assert_eq!(store.list_companion_threads(None).await.unwrap().len(), 2);
        assert_eq!(again.companion_id, "companion_9");
        assert_eq!(again.title, "再次注册");

        assert!(store.touch_companion_thread("missing", None).await.is_err());
        store.delete_companion_thread("conv_a").await.unwrap();
        assert!(!store.is_companion_thread("conv_a").await.unwrap());
        assert_eq!(store.list_companion_threads(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn v0_database_migrates_to_v1() {
        // Hand-build a v0 database: companion_threads without companion_id,
        // user_version still 0.
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::raw_sql(
            r#"
            CREATE TABLE companion_threads (
              conversation_id TEXT PRIMARY KEY,
              title TEXT NOT NULL DEFAULT '',
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );
            INSERT INTO companion_threads(conversation_id, title, created_at, updated_at)
              VALUES('conv_old', '旧线程', 1, 1);
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        apply_migrations(&pool).await.unwrap();

        let cols: Vec<String> = sqlx::query("PRAGMA table_info(companion_threads)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.contains(&"companion_id".to_string()), "companion_id column missing: {cols:?}");
        let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_VERSION);
        // Legacy row got the empty-string default.
        let companion_id: String = sqlx::query("SELECT companion_id FROM companion_threads WHERE conversation_id = 'conv_old'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("companion_id");
        assert_eq!(companion_id, "");

        // Idempotent: a second run is a no-op.
        apply_migrations(&pool).await.unwrap();
        let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_VERSION);
    }

    #[tokio::test]
    async fn v0_file_database_migrates_through_open() {
        // End-to-end: a pre-existing on-disk v0 memory.db (old table shape,
        // user_version 0, one legacy row) opened via CompanionStore::open must come
        // out migrated with the legacy row intact.
        let dir = tempfile::tempdir().unwrap();
        {
            let opts = SqliteConnectOptions::new()
                .filename(dir.path().join("memory.db"))
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();
            sqlx::raw_sql(
                r#"
                CREATE TABLE companion_threads (
                  conversation_id TEXT PRIMARY KEY,
                  title TEXT NOT NULL DEFAULT '',
                  created_at INTEGER NOT NULL,
                  updated_at INTEGER NOT NULL
                );
                INSERT INTO companion_threads(conversation_id, title, created_at, updated_at)
                  VALUES('conv_old', '旧线程', 1, 1);
                PRAGMA user_version = 0;
                "#,
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let store = CompanionStore::open(dir.path()).await.unwrap();

        let cols: Vec<String> = sqlx::query("PRAGMA table_info(companion_threads)")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.contains(&"companion_id".to_string()), "companion_id column missing: {cols:?}");
        let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_VERSION);
        // Legacy row survives and reads back with the empty-string companion_id.
        let threads = store.list_companion_threads(None).await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].conversation_id, "conv_old");
        assert_eq!(threads[0].title, "旧线程");
        assert_eq!(threads[0].companion_id, "");
    }

    #[tokio::test]
    async fn v1_database_dedupes_companion_threads_and_adds_unique_index() {
        // A v1 db whose companion_threads holds DUPLICATE companion_ids (the
        // pre-single-session world). The v1→v2 rung must keep the
        // most-recently-updated thread per companion, drop the rest (registry rows
        // only), then enforce the partial UNIQUE INDEX.
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // Build a v1-shaped table (already has companion_id, no unique index) with
        // three threads for companion_1 (keep the newest = conv_a3), one for companion_2,
        // and two legacy empty-companion_id rows (exempt — must both survive).
        sqlx::raw_sql(
            r#"
            CREATE TABLE companion_threads (
              conversation_id TEXT PRIMARY KEY,
              companion_id TEXT NOT NULL DEFAULT '',
              title TEXT NOT NULL DEFAULT '',
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );
            INSERT INTO companion_threads(conversation_id, companion_id, title, created_at, updated_at) VALUES
              ('conv_a1', 'companion_1', '甲一', 1, 10),
              ('conv_a2', 'companion_1', '甲二', 1, 20),
              ('conv_a3', 'companion_1', '甲三', 1, 30),
              ('conv_b1', 'companion_2', '乙一', 1, 5),
              ('conv_l1', '', '旧一', 1, 1),
              ('conv_l2', '', '旧二', 1, 2);
            PRAGMA user_version = 1;
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        apply_migrations(&pool).await.unwrap();

        let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_VERSION);

        let store = CompanionStore { pool: pool.clone() };
        // companion_1 keeps only the newest thread (conv_a3); companion_2 keeps its one.
        let companion1 = store.list_companion_threads(Some("companion_1")).await.unwrap();
        assert_eq!(companion1.len(), 1, "companion_1 must be deduped to a single thread");
        assert_eq!(companion1[0].conversation_id, "conv_a3");
        let companion2 = store.list_companion_threads(Some("companion_2")).await.unwrap();
        assert_eq!(companion2.len(), 1);
        assert_eq!(companion2[0].conversation_id, "conv_b1");
        // Empty-companion_id legacy rows are exempt and both survive.
        let all = store.list_companion_threads(None).await.unwrap();
        let legacy: Vec<_> = all.iter().filter(|t| t.companion_id.is_empty()).collect();
        assert_eq!(legacy.len(), 2, "empty-companion_id rows must not be deduped");

        // The UNIQUE INDEX now blocks a second non-empty thread for a companion.
        let dup = sqlx::query(
            "INSERT INTO companion_threads(conversation_id, companion_id, title, created_at, updated_at) VALUES('conv_a4','companion_1','重复',1,40)",
        )
        .execute(&pool)
        .await;
        assert!(dup.is_err(), "unique index must reject a second thread for companion_1");
        // …but multiple empty-companion_id rows remain allowed (partial index).
        sqlx::query(
            "INSERT INTO companion_threads(conversation_id, companion_id, title, created_at, updated_at) VALUES('conv_l3','','旧三',1,3)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Idempotent: a second migration run is a no-op.
        apply_migrations(&pool).await.unwrap();
        assert_eq!(
            store.list_companion_threads(Some("companion_1")).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn fresh_file_database_is_stamped_current() {
        // A brand-new on-disk db must be born at STORE_VERSION (no migration
        // rung ever runs against it) and survive a reopen unchanged.
        let dir = tempfile::tempdir().unwrap();
        {
            let store = CompanionStore::open(dir.path()).await.unwrap();
            let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(version, STORE_VERSION);
            store.pool.close().await;
        }
        let store = CompanionStore::open(dir.path()).await.unwrap();
        let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_VERSION);
    }

    #[tokio::test]
    async fn per_companion_xp_and_state_kv() {
        let store = CompanionStore::open_memory().await.unwrap();
        // Missing keys default to 0.
        assert_eq!(store.get_companion_state_i64("companion_1", "xp").await.unwrap(), 0);

        assert_eq!(store.add_companion_xp("companion_1", 5).await.unwrap(), 5);
        assert_eq!(store.add_companion_xp("companion_1", 7).await.unwrap(), 12);
        assert_eq!(store.get_companion_state_i64("companion_1", "xp").await.unwrap(), 12);
        // Other companions are untouched.
        assert_eq!(store.get_companion_state_i64("companion_2", "xp").await.unwrap(), 0);

        store.add_xp_all(&["companion_1".into(), "companion_2".into()], 3).await.unwrap();
        assert_eq!(store.get_companion_state_i64("companion_1", "xp").await.unwrap(), 15);
        assert_eq!(store.get_companion_state_i64("companion_2", "xp").await.unwrap(), 3);

        store.set_companion_state("companion_1", "mood", "happy").await.unwrap();
        assert_eq!(store.get_companion_state("companion_1", "mood").await.unwrap().as_deref(), Some("happy"));
        assert_eq!(store.get_companion_state("companion_2", "mood").await.unwrap(), None);
    }

    #[tokio::test]
    async fn thread_companion_id_hit_miss_and_empty() {
        let store = CompanionStore::open_memory().await.unwrap();
        store.insert_companion_thread("conv_a", "companion_1", "甲").await.unwrap();
        store.insert_companion_thread("conv_legacy", "", "旧").await.unwrap();

        assert_eq!(store.thread_companion_id("conv_a").await.unwrap().as_deref(), Some("companion_1"));
        assert_eq!(store.thread_companion_id("conv_missing").await.unwrap(), None);
        // Legacy empty-string companion_id reads back as None.
        assert_eq!(store.thread_companion_id("conv_legacy").await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_companion_rows_only_targets_one_companion() {
        let store = CompanionStore::open_memory().await.unwrap();
        store.add_companion_xp("companion_1", 10).await.unwrap();
        store.add_companion_xp("companion_2", 20).await.unwrap();
        store.set_companion_state("companion_1", "mood", "happy").await.unwrap();
        store.insert_companion_thread("conv_a", "companion_1", "甲").await.unwrap();
        store.insert_companion_thread("conv_b", "companion_2", "乙").await.unwrap();

        store.delete_companion_rows("companion_1").await.unwrap();

        assert_eq!(store.get_companion_state_i64("companion_1", "xp").await.unwrap(), 0);
        assert_eq!(store.get_companion_state("companion_1", "mood").await.unwrap(), None);
        assert!(!store.is_companion_thread("conv_a").await.unwrap());
        // companion_2 keeps everything.
        assert_eq!(store.get_companion_state_i64("companion_2", "xp").await.unwrap(), 20);
        assert!(store.is_companion_thread("conv_b").await.unwrap());
    }

    #[tokio::test]
    async fn backfill_first_companion_claims_single_legacy_row_and_kv() {
        let store = CompanionStore::open_memory().await.unwrap();
        // Two legacy threads with empty companion_id + one already owned by another
        // companion. Hand-set updated_at so the keeper is deterministic: conv_b is
        // the newest empty-companion_id row.
        store.insert_companion_thread("conv_a", "", "旧甲").await.unwrap();
        store.insert_companion_thread("conv_b", "", "旧乙").await.unwrap();
        store.insert_companion_thread("conv_c", "companion_other", "新").await.unwrap();
        sqlx::query("UPDATE companion_threads SET updated_at = 10 WHERE conversation_id = 'conv_a'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE companion_threads SET updated_at = 20 WHERE conversation_id = 'conv_b'")
            .execute(&store.pool)
            .await
            .unwrap();
        // Legacy global kv.
        store.set_state("xp", "120").await.unwrap();
        store.set_state("companion_active_thread", "conv_b").await.unwrap();

        store.backfill_first_companion("companion_first").await.unwrap();

        // Single-session invariant: only the newest legacy row (conv_b) is
        // claimed; the surplus (conv_a) is dropped from the registry. The
        // companion owns exactly one thread. Owned rows of other companions untouched.
        assert_eq!(store.thread_companion_id("conv_b").await.unwrap().as_deref(), Some("companion_first"));
        assert!(!store.is_companion_thread("conv_a").await.unwrap(), "surplus legacy row must be pruned");
        assert_eq!(store.list_companion_threads(Some("companion_first")).await.unwrap().len(), 1);
        assert_eq!(store.thread_companion_id("conv_c").await.unwrap().as_deref(), Some("companion_other"));
        // Global kv moved into companion_runtime_state…
        assert_eq!(store.get_companion_state_i64("companion_first", "xp").await.unwrap(), 120);
        assert_eq!(
            store.get_companion_state("companion_first", "companion_active_thread").await.unwrap().as_deref(),
            Some("conv_b")
        );
        // …and the residual global keys are gone.
        assert_eq!(store.get_state("xp").await.unwrap(), None);
        assert_eq!(store.get_state("companion_active_thread").await.unwrap(), None);

        // Idempotent: a second run never clobbers values earned since, and the
        // companion still owns exactly one thread (the claim is skipped now that it
        // already owns one).
        store.add_companion_xp("companion_first", 5).await.unwrap();
        store.backfill_first_companion("companion_first").await.unwrap();
        assert_eq!(store.get_companion_state_i64("companion_first", "xp").await.unwrap(), 125);
        assert_eq!(store.list_companion_threads(Some("companion_first")).await.unwrap().len(), 1);
        assert_eq!(store.thread_companion_id("conv_c").await.unwrap().as_deref(), Some("companion_other"));
    }

    #[tokio::test]
    async fn backfill_never_overwrites_existing_per_companion_values() {
        let store = CompanionStore::open_memory().await.unwrap();
        // The companion already has per-companion xp; the stale global key must lose and
        // still be cleaned up.
        store.add_companion_xp("companion_first", 999).await.unwrap();
        store.set_state("xp", "120").await.unwrap();

        store.backfill_first_companion("companion_first").await.unwrap();

        assert_eq!(store.get_companion_state_i64("companion_first", "xp").await.unwrap(), 999);
        assert_eq!(store.get_state("xp").await.unwrap(), None);
    }

    fn raw_memory(id: &str, kind: &str, content: &str, status: &str) -> CompanionMemory {
        CompanionMemory {
            id: id.to_owned(),
            kind: kind.to_owned(),
            content: content.to_owned(),
            tags: vec!["标签".into()],
            importance: 0.7,
            strength: 0.42,
            pinned: true,
            source: "manual".into(),
            status: status.to_owned(),
            created_at: 1_111,
            updated_at: 2_222,
            last_reinforced_at: 3_333,
            scope_kind: "companion".into(),
            scope_companion_id: "c-raw".into(),
        }
    }

    #[tokio::test]
    async fn insert_raw_get_and_dump_preserve_all_fields() {
        let store = CompanionStore::open_memory().await.unwrap();
        let active = raw_memory("mem_aaa", "preference", "主人喜欢深色主题", "active");
        let archived = raw_memory("mem_bbb", "episode", "上周修了导出 bug", "archived");
        store.insert_memory_raw(&active).await.unwrap();
        store.insert_memory_raw(&archived).await.unwrap();

        // get_memory reads back exactly what went in (timestamps incl.).
        let got = store.get_memory("mem_aaa").await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(&got).unwrap(), serde_json::to_value(&active).unwrap());
        assert!(store.get_memory("mem_missing").await.unwrap().is_none());

        // Duplicate id is a hard error (caller decides how to regenerate).
        assert!(store.insert_memory_raw(&active).await.is_err());

        // Dump includes archived rows, ordered by id.
        let dump = store.dump_memories_all().await.unwrap();
        assert_eq!(
            dump.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["mem_aaa", "mem_bbb"]
        );
        assert_eq!(dump[1].status, "archived");
    }

    #[tokio::test]
    async fn dump_memories_pages_past_the_cursor_page_size() {
        let store = CompanionStore::open_memory().await.unwrap();
        let count = (CompanionStore::DUMP_PAGE + 3) as usize;
        for i in 0..count {
            store
                .insert_memory_raw(&raw_memory(&format!("mem_{i:05}"), "knowledge", &format!("内容 {i}"), "active"))
                .await
                .unwrap();
        }
        let dump = store.dump_memories_all().await.unwrap();
        assert_eq!(dump.len(), count);
        assert_eq!(dump.first().unwrap().id, "mem_00000");
        assert_eq!(dump.last().unwrap().id, format!("mem_{:05}", count - 1));
    }

    #[tokio::test]
    async fn learn_run_dump_and_exists() {
        let store = CompanionStore::open_memory().await.unwrap();
        assert!(!store.learn_run_exists("run_x").await.unwrap());
        store
            .insert_learn_run(&CompanionLearnRun {
                id: "run_x".into(),
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
        assert!(store.learn_run_exists("run_x").await.unwrap());

        let dump = store.dump_learn_runs_all().await.unwrap();
        assert_eq!(dump.len(), 1);
        assert_eq!(dump[0].id, "run_x");
        assert_eq!(dump[0].finished_at, Some(20));
        assert_eq!(dump[0].summary.as_deref(), Some("学到了"));
    }

    #[tokio::test]
    async fn open_registers_the_live_store() {
        let dir = tempfile::tempdir().unwrap();
        let _store = CompanionStore::open(dir.path()).await.unwrap();
        // First-wins across parallel tests — only presence is deterministic.
        assert!(live_store().is_some());
    }
}
