use nomifun_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::TerminalSessionRow;
use crate::repository::terminal::{CreateTerminalParams, ITerminalRepository};

#[derive(Clone, Debug)]
pub struct SqliteTerminalRepository {
    pool: SqlitePool,
}

impl SqliteTerminalRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ITerminalRepository for SqliteTerminalRepository {
    async fn create(&self, params: &CreateTerminalParams) -> Result<TerminalSessionRow, DbError> {
        let now = now_ms();
        // `id` is allocated by SQLite (INTEGER PK AUTOINCREMENT) and never bound.
        let result = sqlx::query(
            "INSERT INTO terminal_sessions (\
                name, cwd, command, args, env, backend, mode, cols, rows, \
                created_at, updated_at, last_status, exit_code, user_id\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&params.name)
        .bind(&params.cwd)
        .bind(&params.command)
        .bind(&params.args)
        .bind(&params.env)
        .bind(&params.backend)
        .bind(&params.mode)
        .bind(params.cols)
        .bind(params.rows)
        .bind(now)
        .bind(now)
        .bind("running")
        .bind(Option::<i64>::None)
        .bind(&params.user_id)
        .execute(&self.pool)
        .await?;

        Ok(TerminalSessionRow {
            id: result.last_insert_rowid(),
            name: params.name.clone(),
            cwd: params.cwd.clone(),
            command: params.command.clone(),
            args: params.args.clone(),
            env: params.env.clone(),
            backend: params.backend.clone(),
            mode: params.mode.clone(),
            cols: params.cols,
            rows: params.rows,
            created_at: now,
            updated_at: now,
            last_status: "running".to_owned(),
            exit_code: None,
            user_id: params.user_id.clone(),
            pinned: false,
            pinned_at: None,
            autowork: None,
            idmm: None,
        })
    }

    async fn get_by_id(&self, id: i64) -> Result<Option<TerminalSessionRow>, DbError> {
        let row = sqlx::query_as::<_, TerminalSessionRow>("SELECT * FROM terminal_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<TerminalSessionRow>, DbError> {
        let rows = sqlx::query_as::<_, TerminalSessionRow>(
            "SELECT * FROM terminal_sessions WHERE user_id = ? \
             ORDER BY pinned DESC, COALESCE(pinned_at, created_at) DESC, created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn update_status(&self, id: i64, last_status: &str, exit_code: Option<i64>) -> Result<(), DbError> {
        let result =
            sqlx::query("UPDATE terminal_sessions SET last_status = ?, exit_code = ?, updated_at = ? WHERE id = ?")
                .bind(last_status)
                .bind(exit_code)
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn mark_all_running_exited(&self) -> Result<u64, DbError> {
        // No id filter and no NotFound: a clean boot with zero ghost rows is the
        // normal case and must not error.
        let result = sqlx::query(
            "UPDATE terminal_sessions SET last_status = 'exited', exit_code = NULL, updated_at = ? \
             WHERE last_status = 'running'",
        )
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn save_scrollback(&self, id: i64, data: &[u8]) -> Result<(), DbError> {
        // UPSERT keyed on the session id (PK). The FK to terminal_sessions means
        // a row for a deleted session can never be written.
        sqlx::query(
            "INSERT INTO terminal_scrollback (session_id, data, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(session_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
        )
        .bind(id)
        .bind(data)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_scrollback(&self, id: i64) -> Result<Option<Vec<u8>>, DbError> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT data FROM terminal_scrollback WHERE session_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(data,)| data))
    }

    async fn clear_scrollback(&self, id: i64) -> Result<(), DbError> {
        // Idempotent: a missing row is fine (relaunch of a session that never
        // had persisted scrollback).
        sqlx::query("DELETE FROM terminal_scrollback WHERE session_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_size(&self, id: i64, cols: i64, rows: i64) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE terminal_sessions SET cols = ?, rows = ?, updated_at = ? WHERE id = ?")
            .bind(cols)
            .bind(rows)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn update_meta(&self, id: i64, name: Option<&str>, pinned: Option<bool>) -> Result<(), DbError> {
        // Build the SET clause from the provided fields. At least `updated_at`
        // is always set, so the query is never empty.
        let now = now_ms();
        let mut sets: Vec<&str> = vec!["updated_at = ?"];
        if name.is_some() {
            sets.push("name = ?");
        }
        if pinned.is_some() {
            sets.push("pinned = ?");
            sets.push("pinned_at = ?");
        }
        let sql = format!("UPDATE terminal_sessions SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql).bind(now);
        if let Some(n) = name {
            q = q.bind(n.to_owned());
        }
        if let Some(p) = pinned {
            q = q.bind(p);
            q = q.bind(if p { Some(now) } else { None });
        }
        let result = q.bind(id).execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn update_command(
        &self,
        id: i64,
        command: &str,
        args: &str,
        backend: Option<&str>,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE terminal_sessions SET command = ?, args = ?, backend = ?, updated_at = ? WHERE id = ?",
        )
        .bind(command)
        .bind(args)
        .bind(backend)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn update_autowork(&self, id: i64, autowork: Option<&str>) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE terminal_sessions SET autowork = ?, updated_at = ? WHERE id = ?")
            .bind(autowork)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn update_idmm(&self, id: i64, idmm: Option<&str>) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE terminal_sessions SET idmm = ?, updated_at = ? WHERE id = ?")
            .bind(idmm)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn get_idmm(&self, id: i64) -> Result<Option<String>, DbError> {
        let row: Option<(Option<String>,)> = sqlx::query_as("SELECT idmm FROM terminal_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(v,)| v))
    }

    async fn delete(&self, id: i64) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM terminal_sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        Ok(())
    }

    async fn delete_all(&self) -> Result<u64, DbError> {
        // Whole-table wipe (no WHERE, no NotFound): a clean exit with zero rows
        // is the normal case. terminal_scrollback is dropped by FK CASCADE.
        let result = sqlx::query("DELETE FROM terminal_sessions").execute(&self.pool).await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    /// The only `users` row seeded by `init_database_memory`. Tests that don't
    /// care about per-user semantics own their terminal sessions with this id so
    /// the `terminal_sessions.user_id → users(id)` FK is satisfied.
    const SYSTEM_USER_ID: &str = "system_default_user";

    /// Inserts a real `users` row so a distinct `user_id` can own terminal
    /// sessions (or be queried as an empty owner) without tripping the FK.
    /// `username` is unique, so we derive it from the id.
    async fn seed_user(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES (?, ?, 'hash', 0, 0)",
        )
        .bind(id)
        .bind(format!("u_{id}"))
        .execute(pool)
        .await
        .unwrap();
    }

    fn params(user: &str) -> CreateTerminalParams {
        CreateTerminalParams {
            name: "shell".into(),
            cwd: "/tmp".into(),
            command: "$SHELL".into(),
            args: "[]".into(),
            env: None,
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            user_id: user.to_owned(),
        }
    }

    #[tokio::test]
    async fn create_get_list_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        seed_user(db.pool(), "user_a").await;

        let created = repo.create(&params("user_a")).await.unwrap();
        assert!(created.id > 0);
        assert_eq!(created.last_status, "running");

        let got = repo.get_by_id(created.id).await.unwrap().unwrap();
        assert_eq!(got.id, created.id);
        assert_eq!(got.command, "$SHELL");

        let list = repo.list_by_user("user_a").await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn list_is_user_isolated() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        // Distinct owners must be real `users` rows for the FK to hold. `user_c`
        // is seeded too so the "empty owner returns nothing" check queries a
        // genuine, distinct user rather than a non-existent id.
        seed_user(db.pool(), "user_a").await;
        seed_user(db.pool(), "user_b").await;
        seed_user(db.pool(), "user_c").await;
        repo.create(&params("user_a")).await.unwrap();
        repo.create(&params("user_b")).await.unwrap();

        assert_eq!(repo.list_by_user("user_a").await.unwrap().len(), 1);
        assert_eq!(repo.list_by_user("user_b").await.unwrap().len(), 1);
        assert_eq!(repo.list_by_user("user_c").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn update_status_and_size() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;

        repo.update_status(id, "exited", Some(0)).await.unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(got.last_status, "exited");
        assert_eq!(got.exit_code, Some(0));

        repo.update_size(id, 120, 40).await.unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!((got.cols, got.rows), (120, 40));
    }

    #[tokio::test]
    async fn update_autowork_roundtrips_and_clears() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;

        // default is NULL
        assert!(repo.get_by_id(id).await.unwrap().unwrap().autowork.is_none());

        repo.update_autowork(id, Some(r#"{"enabled":true,"tag":"alpha"}"#))
            .await
            .unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(got.autowork.as_deref(), Some(r#"{"enabled":true,"tag":"alpha"}"#));

        repo.update_autowork(id, None).await.unwrap();
        assert!(repo.get_by_id(id).await.unwrap().unwrap().autowork.is_none());

        assert!(matches!(
            repo.update_autowork(999_999, Some("{}")).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn update_and_delete_missing_returns_not_found() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        assert!(matches!(
            repo.update_status(999_999, "exited", None).await.unwrap_err(),
            DbError::NotFound(_)
        ));
        assert!(matches!(repo.delete(999_999).await.unwrap_err(), DbError::NotFound(_)));
        assert!(repo.get_by_id(999_999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;
        repo.delete(id).await.unwrap();
        assert!(repo.get_by_id(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_all_clears_rows_and_scrollback_cascade() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let a = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;
        let b = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;
        repo.save_scrollback(a, b"history-a").await.unwrap();
        repo.save_scrollback(b, b"history-b").await.unwrap();

        let n = repo.delete_all().await.unwrap();
        assert_eq!(n, 2, "both session rows must be deleted");
        assert!(repo.list_by_user(SYSTEM_USER_ID).await.unwrap().is_empty());
        // FK ON DELETE CASCADE wipes the scrollback rows too.
        assert!(repo.load_scrollback(a).await.unwrap().is_none());
        assert!(repo.load_scrollback(b).await.unwrap().is_none());

        // Idempotent: a second wipe on an empty table deletes nothing and is OK.
        assert_eq!(repo.delete_all().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn update_command_rewrites_launch_identity() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        // Start as an agent CLI, then fall back to a shell in place.
        let mut p = params(SYSTEM_USER_ID);
        p.command = "claude".into();
        p.backend = Some("claude".into());
        let id = repo.create(&p).await.unwrap().id;

        repo.update_command(id, "$SHELL", "[]", None).await.unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(got.command, "$SHELL");
        assert_eq!(got.args, "[]");
        assert_eq!(got.backend, None);

        assert!(matches!(
            repo.update_command(999_999, "$SHELL", "[]", None).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn update_meta_renames_and_pins() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;

        // rename only
        repo.update_meta(id, Some("My Term"), None).await.unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(got.name, "My Term");
        assert!(!got.pinned);

        // pin only (name preserved)
        repo.update_meta(id, None, Some(true)).await.unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(got.name, "My Term");
        assert!(got.pinned);
        assert!(got.pinned_at.is_some());

        // unpin clears pinned_at
        repo.update_meta(id, None, Some(false)).await.unwrap();
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert!(!got.pinned);
        assert!(got.pinned_at.is_none());

        assert!(matches!(
            repo.update_meta(999_999, Some("x"), None).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_orders_pinned_first() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let a = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;
        repo.create(&params(SYSTEM_USER_ID)).await.unwrap();
        repo.create(&params(SYSTEM_USER_ID)).await.unwrap();
        // pin the oldest → it should jump to the front.
        repo.update_meta(a, None, Some(true)).await.unwrap();
        let list = repo.list_by_user(SYSTEM_USER_ID).await.unwrap();
        assert_eq!(list.first().unwrap().id, a);
    }

    #[tokio::test]
    async fn idmm_column_roundtrips() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;

        // default is NULL
        assert!(repo.get_by_id(id).await.unwrap().unwrap().idmm.is_none());
        assert!(repo.get_idmm(id).await.unwrap().is_none());

        // set IDMM config
        let cfg = r#"{"enabled":true}"#;
        repo.update_idmm(id, Some(cfg)).await.unwrap();

        // verify via full row getter
        let got = repo.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(got.idmm.as_deref(), Some(cfg));

        // verify via get_idmm
        assert_eq!(repo.get_idmm(id).await.unwrap().as_deref(), Some(cfg));

        // clear
        repo.update_idmm(id, None).await.unwrap();
        assert!(repo.get_by_id(id).await.unwrap().unwrap().idmm.is_none());
        assert!(repo.get_idmm(id).await.unwrap().is_none());

        // not-found on missing session
        assert!(matches!(
            repo.update_idmm(999_999, Some("{}")).await.unwrap_err(),
            DbError::NotFound(_)
        ));

        // get_idmm on missing session returns None (not an error)
        assert!(repo.get_idmm(999_999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mark_all_running_exited_flips_only_running() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let a = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id; // running
        let b = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id; // running
        let c = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;
        // `c` exited before boot — reconciliation must leave it (and its code) be.
        repo.update_status(c, "exited", Some(7)).await.unwrap();

        let n = repo.mark_all_running_exited().await.unwrap();
        assert_eq!(n, 2, "only the two running rows are reconciled");

        for id in [a, b] {
            let row = repo.get_by_id(id).await.unwrap().unwrap();
            assert_eq!(row.last_status, "exited");
            assert_eq!(row.exit_code, None);
        }
        let row_c = repo.get_by_id(c).await.unwrap().unwrap();
        assert_eq!(row_c.last_status, "exited");
        assert_eq!(row_c.exit_code, Some(7), "pre-exited row left untouched");

        // Idempotent: a second boot pass reconciles nothing.
        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn scrollback_save_load_clear_and_cascade() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(SYSTEM_USER_ID)).await.unwrap().id;

        // absent → None
        assert!(repo.load_scrollback(id).await.unwrap().is_none());

        // save → load roundtrip (binary-safe: contains an ESC sequence + NUL)
        let payload = b"hello\x1b[0m\x00 world";
        repo.save_scrollback(id, payload).await.unwrap();
        assert_eq!(repo.load_scrollback(id).await.unwrap().as_deref(), Some(&payload[..]));

        // UPSERT overwrites in place
        repo.save_scrollback(id, b"newer").await.unwrap();
        assert_eq!(repo.load_scrollback(id).await.unwrap().as_deref(), Some(&b"newer"[..]));

        // clear → None, and idempotent (second clear is a no-op, not an error)
        repo.clear_scrollback(id).await.unwrap();
        assert!(repo.load_scrollback(id).await.unwrap().is_none());
        repo.clear_scrollback(id).await.unwrap();

        // FK ON DELETE CASCADE: deleting the session drops its scrollback row.
        repo.save_scrollback(id, b"persisted").await.unwrap();
        repo.delete(id).await.unwrap();
        assert!(
            repo.load_scrollback(id).await.unwrap().is_none(),
            "scrollback row must be CASCADE-removed when its session is deleted"
        );
    }
}
