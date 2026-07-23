use nomifun_common::{ProviderId, now_ms};
use sqlx::{Sqlite, SqlitePool, Transaction};

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

fn idmm_bypass_provider_ids(encoded: &str) -> Result<Vec<String>, DbError> {
    let value: serde_json::Value = serde_json::from_str(encoded)
        .map_err(|error| DbError::Conflict(format!("IDMM config is invalid JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| DbError::Conflict("IDMM config must be a JSON object".to_owned()))?;

    let mut provider_ids = Vec::new();
    for watch in ["fault_watch", "decision_watch"] {
        let Some(bypass_model) = object.get(watch).and_then(|watch| watch.get("bypass_model"))
        else {
            continue;
        };
        let bypass_model = bypass_model.as_object().ok_or_else(|| {
            DbError::Conflict(format!("IDMM {watch}.bypass_model must be an object"))
        })?;
        let Some(provider_id) = bypass_model.get("provider_id") else {
            continue;
        };
        if provider_id.is_null() {
            continue;
        }
        let provider_id = provider_id.as_str().ok_or_else(|| {
            DbError::Conflict(format!(
                "IDMM {watch}.bypass_model.provider_id must be a string"
            ))
        })?;
        ProviderId::parse(provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "IDMM {watch}.bypass_model.provider_id is not canonical: {error}"
            ))
        })?;
        provider_ids.push(provider_id.to_owned());
    }
    provider_ids.sort_unstable();
    provider_ids.dedup();
    Ok(provider_ids)
}

async fn lock_idmm_bypass_providers(
    tx: &mut Transaction<'_, Sqlite>,
    idmm: Option<&str>,
) -> Result<(), DbError> {
    let Some(idmm) = idmm else {
        return Ok(());
    };
    for provider_id in idmm_bypass_provider_ids(idmm)? {
        let locked = sqlx::query(
            "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
        )
        .bind(&provider_id)
        .execute(&mut **tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "IDMM references missing provider '{provider_id}'"
            )));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl ITerminalRepository for SqliteTerminalRepository {
    async fn create(&self, params: &CreateTerminalParams) -> Result<TerminalSessionRow, DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let owner = sqlx::query(
            "UPDATE users SET updated_at = updated_at WHERE user_id = ?",
        )
        .bind(params.user_id.as_str())
        .execute(&mut *tx)
        .await?;
        if owner.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "terminal owner '{}'",
                params.user_id
            )));
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO terminal_sessions (\
                terminal_id, name, cwd, command, args, env, backend, mode, cols, rows, \
                created_at, updated_at, last_status, exit_code, user_id, pinned, pinned_at, autowork, idmm\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', NULL, ?, 0, NULL, NULL, NULL) RETURNING id",
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
        .bind(params.user_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(TerminalSessionRow {
            id,
            terminal_id: params.id.clone(),
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
        let row = sqlx::query_as::<_, TerminalSessionRow>(
            "SELECT * FROM terminal_sessions WHERE terminal_id = ?",
        )
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

    async fn list_all(&self) -> Result<Vec<TerminalSessionRow>, DbError> {
        Ok(sqlx::query_as::<_, TerminalSessionRow>(
            "SELECT * FROM terminal_sessions ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_status(&self, id: &str, last_status: &str, exit_code: Option<i64>) -> Result<(), DbError> {
        let result =
            sqlx::query("UPDATE terminal_sessions SET last_status = ?, exit_code = ?, updated_at = ? WHERE terminal_id = ?")
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
        let mut tx = self.pool.begin().await?;
        let session = sqlx::query(
            "UPDATE terminal_sessions SET updated_at = updated_at WHERE terminal_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if session.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        // The repository owns this logical relation: only an existing terminal
        // may receive scrollback, and the stable terminal_id is the UPSERT key.
        sqlx::query(
            "INSERT INTO terminal_scrollback (terminal_id, data, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(terminal_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
        )
        .bind(id)
        .bind(data)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn load_scrollback(&self, id: &str) -> Result<Option<Vec<u8>>, DbError> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT data FROM terminal_scrollback WHERE terminal_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(data,)| data))
    }

    async fn clear_scrollback(&self, id: &str) -> Result<(), DbError> {
        // Idempotent: a missing row is fine (relaunch of a session that never
        // had persisted scrollback).
        sqlx::query("DELETE FROM terminal_scrollback WHERE terminal_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_size(&self, id: &str, cols: i64, rows: i64) -> Result<(), DbError> {
        let result = sqlx::query("UPDATE terminal_sessions SET cols = ?, rows = ?, updated_at = ? WHERE terminal_id = ?")
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
        let sql = format!("UPDATE terminal_sessions SET {} WHERE terminal_id = ?", sets.join(", "));
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
            "UPDATE terminal_sessions SET command = ?, args = ?, backend = ?, updated_at = ? WHERE terminal_id = ?",
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
             WHERE terminal_id = ?",
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
        let result = sqlx::query("UPDATE terminal_sessions SET autowork = ?, updated_at = ? WHERE terminal_id = ?")
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
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE terminal_sessions SET updated_at = updated_at WHERE terminal_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }
        lock_idmm_bypass_providers(&mut tx, idmm).await?;
        sqlx::query(
            "UPDATE terminal_sessions SET idmm = ?, updated_at = ? WHERE terminal_id = ?",
        )
        .bind(idmm)
        .bind(now_ms())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_idmm(&self, id: &str) -> Result<Option<String>, DbError> {
        let row: Option<(Option<String>,)> = sqlx::query_as("SELECT idmm FROM terminal_sessions WHERE terminal_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(v,)| v))
    }

    async fn delete(&self, id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(
            "UPDATE terminal_sessions \
             SET updated_at = updated_at \
             WHERE terminal_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("terminal session '{id}'")));
        }

        sqlx::query("DELETE FROM terminal_scrollback WHERE terminal_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM knowledge_binding_bases \
             WHERE knowledge_binding_id IN (\
                SELECT knowledge_binding_id FROM knowledge_bindings \
                WHERE target_kind = 'terminal' AND target_terminal_id = ?\
             )",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM knowledge_bindings \
             WHERE target_kind = 'terminal' AND target_terminal_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE requirements \
             SET status = CASE WHEN status = 'in_progress' THEN 'pending' ELSE status END, \
                 active_turn_started_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE active_turn_started_at \
                 END, \
                 lease_expires_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE lease_expires_at \
                 END, \
                 owner_terminal_id = NULL \
             WHERE owner_terminal_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM idmm_interventions \
             WHERE target_kind = 'terminal' AND target_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM terminal_sessions WHERE terminal_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_all(&self) -> Result<u64, DbError> {
        // Whole-table wipe (no WHERE, no NotFound): a clean exit with zero rows
        // is the normal case. Use the same logical cleanup set as delete().
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM terminal_scrollback")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM knowledge_binding_bases \
             WHERE knowledge_binding_id IN (\
                SELECT knowledge_binding_id FROM knowledge_bindings \
                WHERE target_kind = 'terminal'\
             )",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM knowledge_bindings WHERE target_kind = 'terminal'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE requirements \
             SET status = CASE WHEN status = 'in_progress' THEN 'pending' ELSE status END, \
                 active_turn_started_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE active_turn_started_at \
                 END, \
                 lease_expires_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE lease_expires_at \
                 END, \
                 owner_terminal_id = NULL \
             WHERE owner_terminal_id IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM idmm_interventions WHERE target_kind = 'terminal'",
        )
        .execute(&mut *tx)
        .await?;
        let result = sqlx::query("DELETE FROM terminal_sessions")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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
        assert!(TerminalId::parse(created.terminal_id.as_str()).is_ok());
        assert_eq!(created.terminal_id.as_str().len(), 36);
        assert_eq!(created.last_status, "running");

        repo.update_status(created.terminal_id.as_str(), "exited", Some(0))
            .await
            .unwrap();
        repo.update_size(created.terminal_id.as_str(), 120, 40).await.unwrap();
        repo.update_meta(created.terminal_id.as_str(), Some("renamed"), Some(true))
            .await
            .unwrap();
        let row = repo
            .get_by_id(created.terminal_id.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.last_status, "exited");
        assert_eq!(row.exit_code, Some(0));
        assert_eq!((row.cols, row.rows), (120, 40));
        assert_eq!(row.name, "renamed");
        assert!(row.pinned);

        repo.delete(created.terminal_id.as_str()).await.unwrap();
        assert!(repo
            .get_by_id(created.terminal_id.as_str())
            .await
            .unwrap()
            .is_none());

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
        let mut connection = db.pool().acquire().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO terminal_sessions \
             (terminal_id, name, cwd, command, args, cols, rows, created_at, updated_at, last_status, user_id) \
             VALUES ('term_1', 'bad', '/tmp', '$SHELL', '[]', 80, 24, 1, 1, 'exited', ?)",
        )
        .bind(&owner)
        .execute(&mut *connection)
        .await
        .unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);

        let repo = SqliteTerminalRepository::new(db.pool().clone());
        assert!(repo.list_by_user(&owner).await.is_err());
    }

    #[tokio::test]
    async fn metadata_and_runtime_config_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(&owner)).await.unwrap().terminal_id;

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
    async fn update_idmm_requires_existing_canonical_bypass_providers_atomically() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(&owner)).await.unwrap().terminal_id;
        let existing = r#"{"fault_watch":{"enabled":true,"scan_interval_secs":15}}"#;
        repo.update_idmm(id.as_str(), Some(existing)).await.unwrap();

        let missing = serde_json::json!({
            "fault_watch": {
                "bypass_model": {
                    "provider_id": "0190f5fe-7c00-7a00-8000-000000000099",
                    "model": "missing-model"
                }
            }
        })
        .to_string();
        assert!(matches!(
            repo.update_idmm(id.as_str(), Some(&missing))
                .await
                .unwrap_err(),
            DbError::Conflict(ref message) if message.contains("missing provider")
        ));
        assert_eq!(
            repo.get_idmm(id.as_str()).await.unwrap().as_deref(),
            Some(existing),
            "a rejected IDMM reference must leave the old terminal blob intact"
        );

        let malformed = serde_json::json!({
            "decision_watch": {
                "bypass_model": {
                    "provider_id": "provider-not-a-uuid",
                    "model": "bad-model"
                }
            }
        })
        .to_string();
        assert!(matches!(
            repo.update_idmm(id.as_str(), Some(&malformed))
                .await
                .unwrap_err(),
            DbError::Conflict(ref message) if message.contains("not canonical")
        ));

        let provider_id = "0190f5fe-7c00-7a00-8000-000000000097";
        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', ?, 'https://example.invalid', \
                       'encrypted', '[]', 1, '[]', 0, 0)",
        )
        .bind(provider_id)
        .bind(provider_id)
        .execute(db.pool())
        .await
        .unwrap();
        let valid = serde_json::json!({
            "decision_watch": {
                "enabled": true,
                "bypass_model": {
                    "provider_id": provider_id,
                    "model": "decision-model"
                }
            }
        })
        .to_string();
        repo.update_idmm(id.as_str(), Some(&valid)).await.unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&repo.get_idmm(id.as_str()).await.unwrap().unwrap()).unwrap();
        assert_eq!(
            stored["decision_watch"]["bypass_model"]["provider_id"],
            provider_id
        );
    }

    #[tokio::test]
    async fn scrollback_roundtrips_and_is_explicitly_deleted_with_session() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let id = repo.create(&params(&owner)).await.unwrap().terminal_id;
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
        let first = repo.create(&params(&owner)).await.unwrap().terminal_id;
        let second = repo.create(&params(&owner)).await.unwrap().terminal_id;
        repo.update_meta(first.as_str(), None, Some(true)).await.unwrap();

        let rows = repo.list_by_user(&owner).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].terminal_id, first);
        assert!(rows[0].pinned);
        assert!(rows.iter().any(|row| row.terminal_id == second));
        assert!(repo.list_by_user("other-user").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn boot_reconciliation_and_delete_all_are_idempotent() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let running = repo.create(&params(&owner)).await.unwrap().terminal_id;
        let exited = repo.create(&params(&owner)).await.unwrap().terminal_id;
        repo.update_status(exited.as_str(), "exited", Some(7))
            .await
            .unwrap();

        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 1);
        let running_row = repo.get_by_id(running.as_str()).await.unwrap().unwrap();
        assert_eq!(running_row.last_status, "exited");
        assert_eq!(running_row.exit_code, None);
        let exited_row = repo.get_by_id(exited.as_str()).await.unwrap().unwrap();
        assert_eq!(exited_row.exit_code, Some(7));
        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 0);

        assert_eq!(repo.delete_all().await.unwrap(), 2);
        assert_eq!(repo.delete_all().await.unwrap(), 0);
    }
}
