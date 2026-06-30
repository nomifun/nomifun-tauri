//! Post-relocation absolute-path rewrite for the main database.
//!
//! The desktop shell (`apps/desktop/src/relocate.rs`) relocates a legacy
//! `<temp>/nomifun-data/Nomi` data dir to the per-user application-data
//! location and leaves a [`RELOCATED_FROM_MARKER`] file (JSON, see
//! [`RelocationMarker`]) in the new data dir. The files move, but absolute
//! paths stored *inside* the database still point at the old root:
//!
//! * `knowledge_bases.root_path` — the knowledge purge guard checks
//!   `starts_with({data_dir}/knowledge)`, so a stale prefix breaks both
//!   mounting and managed-purge.
//! * `conversations.extra` `$.workspace` — workspace association used for
//!   session grouping and workspace reuse.
//! * `terminal_sessions.cwd` — the directory a terminal session relaunches in.
//!
//! This step runs once after the database is open and migrated (end of
//! [`super::init_data_layer`]): it rewrites the old-root prefix to the new
//! data root and then renames the marker to [`RELOCATED_DONE_MARKER`].
//! Matching details:
//!
//! * **Both separator spellings** of the root are tried (backslash and
//!   forward slash), since rows were written by different code paths over
//!   time — and for each spelling the boundary character right after the
//!   prefix may be either `\` or `/` (`Path::join` on Windows appends `\`
//!   even to a `/`-spelled prefix).
//! * **Matching is case-insensitive** (`lower(...)` on the comparison side
//!   only): Windows paths are case-insensitive, so `c:\users\...` rows must
//!   match a `C:\Users\...` root. The replacement side keeps the original
//!   suffix bytes untouched (`?new || substr(col, length(?old) + 1)` — ASCII
//!   case-folding preserves length, so the cut point is exact).
//! * `conversations.extra` rows are guarded by `json_valid(...)` so a single
//!   corrupt JSON blob cannot fail the whole rewrite, and all statements run
//!   inside **one transaction** — the rewrite is all-or-nothing.
//! * A marker whose `old_root` is suspiciously shallow (a drive root or a
//!   single top-level dir) is refused: such a prefix would rewrite half the
//!   database. See [`old_root_is_specific`].
//!
//! Any failure keeps the marker so the next boot retries (the rewrite is
//! idempotent: a rewritten row no longer matches the old prefix). It never
//! fails the boot.

use std::path::Path;

use nomifun_db::Database;
use tracing::{info, warn};

/// Marker written by the desktop relocation into the NEW data dir. Content is
/// a JSON [`RelocationMarker`]. Its presence means "files moved, database
/// paths not yet rewritten".
pub const RELOCATED_FROM_MARKER: &str = ".relocated-from";

/// The marker is renamed to this once the database rewrite completed, closing
/// the one-shot gate.
pub const RELOCATED_DONE_MARKER: &str = ".relocated-done";

/// JSON content of [`RELOCATED_FROM_MARKER`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RelocationMarker {
    /// Absolute path of the data root the files were relocated from.
    pub old_root: String,
    /// ms-epoch timestamp of the file relocation (informational).
    #[serde(default)]
    pub relocated_at_ms: i64,
}

/// Text columns rewritten with plain prefix replacement.
const TEXT_COLUMN_REWRITES: [(&str, &str); 2] = [
    ("knowledge_bases", "root_path"),
    ("terminal_sessions", "cwd"),
];

/// One-shot, idempotent, never-fails-the-boot. See module docs.
pub async fn rewrite_relocated_paths(database: &Database, data_dir: &Path) {
    let marker_path = data_dir.join(RELOCATED_FROM_MARKER);
    if !marker_path.exists() {
        return;
    }

    let marker: RelocationMarker = match std::fs::read_to_string(&marker_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(marker) => marker,
        Err(e) => {
            // Unreadable marker: keep it (a deliberate bug signal — this
            // warns on every boot until someone looks at it) and do nothing.
            warn!(marker = %marker_path.display(), error = %e, "relocation: marker unreadable; skipping path rewrite");
            return;
        }
    };

    let old_root = marker.old_root.trim_end_matches(['/', '\\']);
    let new_root_owned = data_dir.display().to_string();
    let new_root = new_root_owned.trim_end_matches(['/', '\\']);
    if old_root.is_empty() {
        warn!(marker = %marker_path.display(), "relocation: marker has empty old_root; skipping path rewrite");
        return;
    }
    if !old_root_is_specific(old_root) {
        // Keep the marker as a bug signal (warns every boot), same as the
        // unreadable-marker case: rewriting with a drive-root prefix would
        // touch every absolute path in the database.
        warn!(
            marker = %marker_path.display(),
            old_root,
            "relocation: marker old_root is suspiciously shallow; refusing path rewrite"
        );
        return;
    }

    match rewrite_path_prefixes(database.pool(), old_root, new_root).await {
        Ok(rows) => {
            info!(rows, old_root, new_root, "relocation: rewrote absolute paths in database");
            // Close the gate. If this rename fails the next boot re-runs the
            // rewrite, which is a no-op (nothing matches the old prefix any
            // more), and retries the rename.
            let done = data_dir.join(RELOCATED_DONE_MARKER);
            if let Err(e) = std::fs::rename(&marker_path, &done) {
                warn!(error = %e, "relocation: failed to finalize marker; rewrite will re-run (no-op) next boot");
            }
        }
        Err(e) => {
            // Keep the marker: retried on the next boot. Never block startup.
            warn!(error = %e, old_root, "relocation: database path rewrite failed; will retry next boot");
        }
    }
}

/// Guard against a corrupt or hand-crafted marker whose `old_root` is a
/// drive root or a single top-level directory (`C:\`, `C:\Temp`, `/tmp`):
/// used as a rewrite prefix it would match — and mangle — most absolute
/// paths in the database. The legitimate desktop legacy root
/// (`<temp>/nomifun-data/Nomi`) always has at least two non-drive
/// components, on every platform.
fn old_root_is_specific(old_root: &str) -> bool {
    old_root
        .split(['/', '\\'])
        .filter(|seg| !seg.is_empty() && !seg.ends_with(':'))
        .count()
        >= 2
}

/// Rewrite the `old_root` prefix to `new_root` in every known absolute-path
/// location. Matching is case-insensitive and accepts either separator as
/// the boundary character after the prefix; the stored suffix bytes are
/// preserved verbatim (see module docs). All statements run in a single
/// transaction. Returns total rows touched.
pub async fn rewrite_path_prefixes(
    pool: &nomifun_db::SqlitePool,
    old_root: &str,
    new_root: &str,
) -> Result<u64, nomifun_db::sqlx::Error> {
    let mut affected = 0u64;
    // One transaction for everything: a mid-flight failure must not leave
    // some tables rewritten and others not — the marker stays and the whole
    // rewrite re-runs on the next boot from a consistent state.
    let mut tx = pool.begin().await?;
    for (old, new) in prefix_variants(old_root, new_root) {
        // Plain text columns. Prefix matching uses exact `substr` comparison
        // (no LIKE — backslashes and underscores in Windows paths would need
        // escaping), wrapped in `lower()` because Windows paths are
        // case-insensitive. It requires either full equality or a path
        // separator (`\` or `/`, mixed spellings happen via Path::join) right
        // after the prefix, so `...\NomiOther` is never mistaken for `...\Nomi`.
        for (table, column) in TEXT_COLUMN_REWRITES {
            let sql = format!(
                "UPDATE {table} SET {column} = ?2 || substr({column}, length(?1) + 1) \
                 WHERE lower({column}) = lower(?1) \
                    OR lower(substr({column}, 1, length(?1) + 1)) IN (lower(?1) || '\\', lower(?1) || '/')"
            );
            affected += nomifun_db::sqlx::query(&sql)
                .bind(&old)
                .bind(&new)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }

        // `conversations.extra` is a JSON object; only its `workspace` key
        // holds an absolute path. `json_set` keeps every other key intact.
        // The `CASE WHEN json_valid(...)` wrapper (NOT a plain `json_valid()
        // AND ...` — SQLite may reorder AND operands, CASE branches are
        // guaranteed lazy) turns corrupt blobs into NULL so a single bad row
        // neither errors the UPDATE nor blocks the rewrite of valid rows.
        affected += nomifun_db::sqlx::query(
            "UPDATE conversations SET extra = json_set(extra, '$.workspace', \
                 ?2 || substr(json_extract(extra, '$.workspace'), length(?1) + 1)) \
             WHERE lower(json_extract(CASE WHEN json_valid(extra) THEN extra END, '$.workspace')) = lower(?1) \
                OR lower(substr(json_extract(CASE WHEN json_valid(extra) THEN extra END, '$.workspace'), \
                                1, length(?1) + 1)) \
                       IN (lower(?1) || '\\', lower(?1) || '/')",
        )
        .bind(&old)
        .bind(&new)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

/// The (old, new) replacement pairs to run: the backslash spelling and the
/// forward-slash spelling of the same roots. Rows were written by different
/// code paths over time (`Path::display` on Windows yields `\`,
/// frontend-supplied or normalized paths may use `/`), so both forms can
/// exist in one database. Each stored path keeps its own suffix verbatim.
fn prefix_variants(old_root: &str, new_root: &str) -> Vec<(String, String)> {
    let candidates = [
        (old_root.replace('/', "\\"), new_root.replace('/', "\\")),
        (old_root.replace('\\', "/"), new_root.replace('\\', "/")),
    ];
    let mut out: Vec<(String, String)> = Vec::new();
    for cand in candidates {
        // Skip degenerate (old == new) and duplicate variants (on Unix both
        // spellings of a `/`-only path collapse into one).
        if cand.0 != cand.1 && !out.iter().any(|v| v.0 == cand.0) {
            out.push(cand);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::sqlx;

    const OLD: &str = r"C:\Users\u\AppData\Local\Temp\nomifun-data\Nomi";
    const NEW: &str = r"C:\Users\u\AppData\Local\NomiFun\Nomi";
    const OLD_FS: &str = "C:/Users/u/AppData/Local/Temp/nomifun-data/Nomi";
    const NEW_FS: &str = "C:/Users/u/AppData/Local/NomiFun/Nomi";

    async fn insert_kb(pool: &nomifun_db::SqlitePool, id: &str, root_path: &str) {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, name, description, root_path, managed, extra, created_at, updated_at) \
             VALUES (?, ?, '', ?, 1, '{}', 0, 0)",
        )
        .bind(id)
        .bind(id)
        .bind(root_path)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn kb_root(pool: &nomifun_db::SqlitePool, id: &str) -> String {
        sqlx::query_scalar("SELECT root_path FROM knowledge_bases WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn insert_conversation(pool: &nomifun_db::SqlitePool, id: i64, extra: &str) {
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, created_at, updated_at) \
             VALUES (?, 'system_default_user', 'c', 'chat', ?, 0, 0)",
        )
        .bind(id)
        .bind(extra)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn conversation_extra(pool: &nomifun_db::SqlitePool, id: i64) -> serde_json::Value {
        let raw: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    async fn insert_terminal(pool: &nomifun_db::SqlitePool, id: i64, cwd: &str) {
        sqlx::query(
            "INSERT INTO terminal_sessions (id, name, cwd, command, created_at, updated_at, user_id) \
             VALUES (?, 't', ?, 'sh', 0, 0, 'system_default_user')",
        )
        .bind(id)
        .bind(cwd)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn terminal_cwd(pool: &nomifun_db::SqlitePool, id: i64) -> String {
        sqlx::query_scalar("SELECT cwd FROM terminal_sessions WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rewrites_both_separator_spellings_without_collateral() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();

        insert_kb(pool, "kb_bs", &format!(r"{OLD}\knowledge\kb_bs")).await;
        insert_kb(pool, "kb_fs", &format!("{OLD_FS}/knowledge/kb_fs")).await;
        insert_kb(pool, "kb_exact", OLD).await;
        // Same leading characters but NOT the old root (`...\NomiX`): the
        // separator-boundary rule must leave it alone.
        insert_kb(pool, "kb_boundary", &format!(r"{OLD}X\f")).await;
        insert_kb(pool, "kb_other", r"D:\somewhere\else").await;

        let rows = rewrite_path_prefixes(pool, OLD, NEW).await.unwrap();
        assert_eq!(rows, 3);

        assert_eq!(kb_root(pool, "kb_bs").await, format!(r"{NEW}\knowledge\kb_bs"));
        assert_eq!(kb_root(pool, "kb_fs").await, format!("{NEW_FS}/knowledge/kb_fs"));
        assert_eq!(kb_root(pool, "kb_exact").await, NEW);
        assert_eq!(kb_root(pool, "kb_boundary").await, format!(r"{OLD}X\f"));
        assert_eq!(kb_root(pool, "kb_other").await, r"D:\somewhere\else");
    }

    #[tokio::test]
    async fn rewrites_conversation_workspace_keeping_other_keys() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();

        let extra = serde_json::json!({
            "workspace": format!(r"{OLD}\conversations\conv_1"),
            "cronJobId": "cj_1",
        });
        insert_conversation(pool, 1, &extra.to_string()).await;
        insert_conversation(pool, 2, "{}").await;
        let unrelated = serde_json::json!({ "workspace": r"D:\projects\mine" });
        insert_conversation(pool, 3, &unrelated.to_string()).await;

        rewrite_path_prefixes(pool, OLD, NEW).await.unwrap();

        let rewritten = conversation_extra(pool, 1).await;
        assert_eq!(rewritten["workspace"], format!(r"{NEW}\conversations\conv_1"));
        assert_eq!(rewritten["cronJobId"], "cj_1");
        assert_eq!(conversation_extra(pool, 2).await, serde_json::json!({}));
        assert_eq!(conversation_extra(pool, 3).await, unrelated);
    }

    #[tokio::test]
    async fn rewrites_terminal_cwd() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();

        insert_terminal(pool, 1, &format!(r"{OLD}\conversations\conv_9")).await;
        insert_terminal(pool, 2, r"C:\repos\work").await;

        rewrite_path_prefixes(pool, OLD, NEW).await.unwrap();

        assert_eq!(terminal_cwd(pool, 1).await, format!(r"{NEW}\conversations\conv_9"));
        assert_eq!(terminal_cwd(pool, 2).await, r"C:\repos\work");
    }

    /// P4: Windows paths are case-insensitive — a row whose stored prefix
    /// differs in case from the marker's `old_root` must still be rewritten,
    /// while the suffix bytes (including their case) are preserved verbatim.
    #[tokio::test]
    async fn case_insensitive_prefix_matches_and_suffix_bytes_survive() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();

        let lowered = OLD.to_lowercase();
        insert_kb(pool, "kb_lower", &format!(r"{lowered}\Knowledge\KB_Mixed")).await;
        insert_kb(pool, "kb_exact_lower", &lowered).await;

        let rows = rewrite_path_prefixes(pool, OLD, NEW).await.unwrap();
        assert_eq!(rows, 2);

        // New prefix in the canonical spelling, original suffix untouched.
        assert_eq!(kb_root(pool, "kb_lower").await, format!(r"{NEW}\Knowledge\KB_Mixed"));
        assert_eq!(kb_root(pool, "kb_exact_lower").await, NEW);
    }

    /// P4: `Path::join` on Windows appends `\` even to a `/`-spelled prefix,
    /// so the boundary separator after the prefix may not match the prefix's
    /// own spelling. Both boundary characters must be accepted per variant.
    #[tokio::test]
    async fn mixed_separator_boundary_is_matched() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();

        // Forward-slash prefix, backslash boundary + suffix.
        insert_kb(pool, "kb_mixed", &format!(r"{OLD_FS}\conversations\conv_1")).await;
        // Still no false positive on a sibling dir sharing the prefix chars.
        insert_kb(pool, "kb_sibling", &format!("{OLD_FS}X/f")).await;

        rewrite_path_prefixes(pool, OLD, NEW).await.unwrap();

        assert_eq!(kb_root(pool, "kb_mixed").await, format!(r"{NEW_FS}\conversations\conv_1"));
        assert_eq!(kb_root(pool, "kb_sibling").await, format!("{OLD_FS}X/f"));
    }

    /// P5: a single row with corrupt JSON in `conversations.extra` must not
    /// fail the UPDATE (CASE/json_valid guard) — the valid rows still rewrite.
    ///
    /// The current schema's expression index (`idx_conversations_cron_job_id`
    /// on `json_extract(extra, ...)`) rejects malformed JSON at INSERT time,
    /// but legacy rows written under older, laxer SQLite JSON parsers (the
    /// 3.45 JSONB rewrite tightened validation) can still sit in real user
    /// databases. Recreate that state by dropping the index for the insert.
    #[tokio::test]
    async fn invalid_json_extra_does_not_poison_the_rewrite() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();

        sqlx::query("DROP INDEX idx_conversations_cron_job_id")
            .execute(pool)
            .await
            .unwrap();
        insert_conversation(pool, 1, "not json {").await;
        let extra = serde_json::json!({ "workspace": format!(r"{OLD}\conversations\conv_ok") });
        insert_conversation(pool, 2, &extra.to_string()).await;

        let rows = rewrite_path_prefixes(pool, OLD, NEW).await.unwrap();
        assert_eq!(rows, 1);

        let rewritten = conversation_extra(pool, 2).await;
        assert_eq!(rewritten["workspace"], format!(r"{NEW}\conversations\conv_ok"));
        let raw_bad: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = 1")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(raw_bad, "not json {");
    }

    #[tokio::test]
    async fn second_pass_is_a_noop() {
        let db = nomifun_db::init_database_memory().await.unwrap();
        let pool = db.pool();
        insert_kb(pool, "kb", &format!(r"{OLD}\knowledge\kb")).await;

        assert_eq!(rewrite_path_prefixes(pool, OLD, NEW).await.unwrap(), 1);
        assert_eq!(rewrite_path_prefixes(pool, OLD, NEW).await.unwrap(), 0);
        assert_eq!(kb_root(pool, "kb").await, format!(r"{NEW}\knowledge\kb"));
    }

    #[tokio::test]
    async fn marker_gates_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let db = nomifun_db::init_database(&data_dir.join("nomifun-backend.db")).await.unwrap();
        // The prefix rewrite targets THIS data dir as the new root.
        insert_kb(db.pool(), "kb", &format!(r"{OLD}\knowledge\kb")).await;

        // No marker → untouched.
        rewrite_relocated_paths(&db, data_dir).await;
        assert_eq!(kb_root(db.pool(), "kb").await, format!(r"{OLD}\knowledge\kb"));

        // Marker present → rewritten + marker renamed to done.
        let marker = serde_json::to_string(&RelocationMarker {
            old_root: OLD.to_string(),
            relocated_at_ms: 1,
        })
        .unwrap();
        std::fs::write(data_dir.join(RELOCATED_FROM_MARKER), &marker).unwrap();
        rewrite_relocated_paths(&db, data_dir).await;
        // The stored row used backslashes, so the backslash spelling of the
        // new root is what lands in the column (platform-independent).
        let new_root_bs = data_dir.display().to_string().trim_end_matches(['/', '\\']).replace('/', "\\");
        let expected = format!(r"{new_root_bs}\knowledge\kb");
        assert_eq!(kb_root(db.pool(), "kb").await, expected);
        assert!(!data_dir.join(RELOCATED_FROM_MARKER).exists());
        assert!(data_dir.join(RELOCATED_DONE_MARKER).exists());

        // Done marker → idempotent no-op.
        rewrite_relocated_paths(&db, data_dir).await;
        assert_eq!(kb_root(db.pool(), "kb").await, expected);
        db.close().await;
    }

    #[tokio::test]
    async fn corrupt_marker_is_kept_and_db_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let db = nomifun_db::init_database(&data_dir.join("nomifun-backend.db")).await.unwrap();
        insert_kb(db.pool(), "kb", &format!(r"{OLD}\knowledge\kb")).await;
        std::fs::write(data_dir.join(RELOCATED_FROM_MARKER), "not json {").unwrap();

        rewrite_relocated_paths(&db, data_dir).await;

        assert_eq!(kb_root(db.pool(), "kb").await, format!(r"{OLD}\knowledge\kb"));
        assert!(data_dir.join(RELOCATED_FROM_MARKER).exists());
        assert!(!data_dir.join(RELOCATED_DONE_MARKER).exists());
        db.close().await;
    }

    /// P7: a marker whose `old_root` is a drive root / single component must
    /// be refused — used as a prefix it would rewrite half the database.
    #[tokio::test]
    async fn shallow_marker_old_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let db = nomifun_db::init_database(&data_dir.join("nomifun-backend.db")).await.unwrap();
        insert_kb(db.pool(), "kb", r"C:\Temp\some\path").await;

        for shallow in [r"C:\", r"C:\Temp", "/tmp"] {
            let marker = serde_json::to_string(&RelocationMarker {
                old_root: shallow.to_string(),
                relocated_at_ms: 1,
            })
            .unwrap();
            std::fs::write(data_dir.join(RELOCATED_FROM_MARKER), &marker).unwrap();
            rewrite_relocated_paths(&db, data_dir).await;

            // Untouched, marker kept (bug signal), no done marker.
            assert_eq!(kb_root(db.pool(), "kb").await, r"C:\Temp\some\path");
            assert!(data_dir.join(RELOCATED_FROM_MARKER).exists());
            assert!(!data_dir.join(RELOCATED_DONE_MARKER).exists());
        }
        db.close().await;
    }

    #[test]
    fn old_root_specificity_matrix() {
        // Too shallow: drive roots and single components.
        for shallow in [r"C:\", "C:", r"C:\Temp", "/", "/tmp", "", r"\\", "//"] {
            assert!(!old_root_is_specific(shallow), "{shallow:?} must be refused");
        }
        // Specific enough: at least two non-drive components.
        for ok in [
            r"C:\Users\u\AppData\Local\Temp\nomifun-data\Nomi",
            "C:/Users/u/AppData/Local/Temp/nomifun-data/Nomi",
            "/tmp/nomifun-data/Nomi",
            r"C:\Temp\Nomi",
        ] {
            assert!(old_root_is_specific(ok), "{ok:?} must be accepted");
        }
    }

    #[test]
    fn variants_deduplicate_unix_style_roots() {
        let variants = prefix_variants("/tmp/nomifun-data/Nomi", "/home/u/.local/share/NomiFun/Nomi");
        // Backslash spelling + forward spelling; both distinct here because
        // the backslash variant mangles separators (it matches nothing real
        // on disk, which is exactly the intent).
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[1].0, "/tmp/nomifun-data/Nomi");
        assert_eq!(variants[1].1, "/home/u/.local/share/NomiFun/Nomi");

        let same = prefix_variants("/same/root", "/same/root");
        assert!(same.is_empty());
    }
}
