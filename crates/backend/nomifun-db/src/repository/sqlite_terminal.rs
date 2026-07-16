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
        sqlx::query(
            "INSERT INTO terminal_sessions (\
                id, name, cwd, command, args, env, backend, mode, cols, rows, \
                created_at, updated_at, last_status, exit_code, user_id\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(params.id.as_str())
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
        .bind(params.user_id.as_str())
        .execute(&self.pool)
        .await?;

        Ok(TerminalSessionRow {
            id: params.id.clone(),
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

    async fn get_by_id(&self, id: &str) -> Result<Option<TerminalSessionRow>, DbError> {
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

    async fn update_status(&self, id: &str, last_status: &str, exit_code: Option<i64>) -> Result<(), DbError> {
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

    async fn save_scrollback(&self, id: &str, data: &[u8]) -> Result<(), DbError> {
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

    async fn load_scrollback(&self, id: &str) -> Result<Option<Vec<u8>>, DbError> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT data FROM terminal_scrollback WHERE session_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(data,)| data))
    }

    async fn clear_scrollback(&self, id: &str) -> Result<(), DbError> {
        // Idempotent: a missing row is fine (relaunch of a session that never
        // had persisted scrollback).
        sqlx::query("DELETE FROM terminal_scrollback WHERE session_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_size(&self, id: &str, cols: i64, rows: i64) -> Result<(), DbError> {
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

    async fn update_meta(&self, id: &str, name: Option<&str>, pinned: Option<bool>) -> Result<(), DbError> {
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
        id: &str,
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

    async fn update_launch_state(
        &self,
        id: &str,
        command: &str,
        args: &str,
        backend: Option<&str>,
        last_status: &str,
        exit_code: Option<i64>,
    ) -> Result<(), DbError> {
        let result = sqlx::query(
            "UPDATE terminal_sessions \
             SET command = ?, args = ?, backend = ?, last_status = ?, \
                 exit_code = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(command)
        .bind(args)
        .bind(backend)
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

    async fn update_autowork(&self, id: &str, autowork: Option<&str>) -> Result<(), DbError> {
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

    async fn update_idmm(&self, id: &str, idmm: Option<&str>) -> Result<(), DbError> {
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

    async fn get_idmm(&self, id: &str) -> Result<Option<String>, DbError> {
        let row: Option<(Option<String>,)> = sqlx::query_as("SELECT idmm FROM terminal_sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(v,)| v))
    }

    async fn delete(&self, id: &str) -> Result<(), DbError> {
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
    use nomifun_common::TerminalId;

    fn params(installation_owner: &str) -> CreateTerminalParams {
        CreateTerminalParams {
            id: TerminalId::new(),
            name: "shell".into(),
            cwd: "/tmp".into(),
            command: "$SHELL".into(),
            args: "[]".into(),
            env: None,
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            user_id: nomifun_common::UserId::parse(installation_owner).unwrap(),
        }
    }

    #[tokio::test]
    async fn create_get_update_and_delete_use_canonical_string_ids() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let created = repo.create(&params(&owner)).await.unwrap();
        assert_eq!(created.id.as_str().split_once('_').unwrap().0, TerminalId::PREFIX);
        assert_eq!(created.last_status, "running");

        repo.update_status(created.id.as_str(), "exited", Some(0))
            .await
            .unwrap();
        repo.update_size(created.id.as_str(), 120, 40).await.unwrap();
        repo.update_meta(created.id.as_str(), Some("renamed"), Some(true))
            .await
            .unwrap();
        let row = repo.get_by_id(created.id.as_str()).await.unwrap().unwrap();
        assert_eq!(row.last_status, "exited");
        assert_eq!(row.exit_code, Some(0));
        assert_eq!((row.cols, row.rows), (120, 40));
        assert_eq!(row.name, "renamed");
        assert!(row.pinned);

        repo.delete(created.id.as_str()).await.unwrap();
        assert!(repo.get_by_id(created.id.as_str()).await.unwrap().is_none());

        let missing = TerminalId::new();
        assert!(matches!(
            repo.update_status(missing.as_str(), "exited", None)
                .await
                .unwrap_err(),
            DbError::NotFound(_)
        ));
        assert!(matches!(
            repo.delete(missing.as_str()).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn malformed_stored_terminal_id_is_rejected_on_read() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO terminal_sessions \
             (id, name, cwd, command, args, cols, rows, created_at, updated_at, last_status, user_id) \
             VALUES ('term_1', 'bad', '/tmp', '$SHELL', '[]', 80, 24, 1, 1, 'exited', ?)",
        )
        .bind(&owner)
        .execute(db.pool())
        .await
        .unwrap();

        let repo = SqliteTerminalRepository::new(db.pool().clone());
        assert!(repo.list_by_user(&owner).await.is_err());
    }

    #[tokio::test]
    async fn metadata_and_runtime_config_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(&owner)).await.unwrap().id;

        repo.update_command(id.as_str(), "claude", r#"["--model","x"]"#, Some("claude"))
            .await
            .unwrap();
        repo.update_autowork(id.as_str(), Some(r#"{"enabled":true,"tag":"alpha"}"#))
            .await
            .unwrap();
        repo.update_idmm(id.as_str(), Some(r#"{"enabled":true}"#))
            .await
            .unwrap();
        let row = repo.get_by_id(id.as_str()).await.unwrap().unwrap();
        assert_eq!(row.command, "claude");
        assert_eq!(row.backend.as_deref(), Some("claude"));
        assert_eq!(
            row.autowork.as_deref(),
            Some(r#"{"enabled":true,"tag":"alpha"}"#)
        );
        assert_eq!(
            repo.get_idmm(id.as_str()).await.unwrap().as_deref(),
            Some(r#"{"enabled":true}"#)
        );

        repo.update_autowork(id.as_str(), None).await.unwrap();
        repo.update_idmm(id.as_str(), None).await.unwrap();
        let row = repo.get_by_id(id.as_str()).await.unwrap().unwrap();
        assert!(row.autowork.is_none());
        assert!(row.idmm.is_none());
    }

    #[tokio::test]
    async fn scrollback_roundtrips_and_cascades_on_delete() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(&owner)).await.unwrap().id;
        let payload = b"hello\x1b[0m\x00 world";

        assert!(repo.load_scrollback(id.as_str()).await.unwrap().is_none());
        repo.save_scrollback(id.as_str(), payload).await.unwrap();
        assert_eq!(
            repo.load_scrollback(id.as_str()).await.unwrap().as_deref(),
            Some(payload.as_slice())
        );
        repo.save_scrollback(id.as_str(), b"newer").await.unwrap();
        assert_eq!(
            repo.load_scrollback(id.as_str()).await.unwrap().as_deref(),
            Some(b"newer".as_slice())
        );
        repo.clear_scrollback(id.as_str()).await.unwrap();
        assert!(repo.load_scrollback(id.as_str()).await.unwrap().is_none());

        repo.save_scrollback(id.as_str(), b"persisted").await.unwrap();
        repo.delete(id.as_str()).await.unwrap();
        assert!(repo.load_scrollback(id.as_str()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_is_user_scoped_and_orders_pinned_first() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let first = repo.create(&params(&owner)).await.unwrap().id;
        let second = repo.create(&params(&owner)).await.unwrap().id;
        repo.update_meta(&first, None, Some(true)).await.unwrap();

        let rows = repo.list_by_user(&owner).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, first);
        assert!(rows[0].pinned);
        assert!(rows.iter().any(|row| row.id == second));
        assert!(repo.list_by_user("other-user").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn boot_reconciliation_and_delete_all_are_idempotent() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let running = repo.create(&params(&owner)).await.unwrap().id;
        let exited = repo.create(&params(&owner)).await.unwrap().id;
        repo.update_status(&exited, "exited", Some(7)).await.unwrap();

        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 1);
        let running_row = repo.get_by_id(&running).await.unwrap().unwrap();
        assert_eq!(running_row.last_status, "exited");
        assert_eq!(running_row.exit_code, None);
        let exited_row = repo.get_by_id(&exited).await.unwrap().unwrap();
        assert_eq!(exited_row.exit_code, Some(7));
        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 0);

        assert_eq!(repo.delete_all().await.unwrap(), 2);
        assert_eq!(repo.delete_all().await.unwrap(), 0);
    }
}
