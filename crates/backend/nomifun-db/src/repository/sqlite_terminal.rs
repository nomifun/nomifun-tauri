use nomifun_common::{ProviderId, RequirementId, TerminalId, generate_id, now_ms};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{TerminalSessionRow, TerminalTurnAdmissionRow};
use crate::repository::terminal::{
    CreateTerminalParams, ITerminalRepository, TerminalTurnAdmissionClaim,
    TerminalTurnAdmissionKey, TerminalTurnAdmissionScope, TerminalTurnEffectsStart,
    TerminalTurnOutcome, TerminalTurnSettlement,
};

#[derive(Clone, Debug)]
pub struct SqliteTerminalRepository {
    pool: SqlitePool,
}

impl SqliteTerminalRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn terminal_turn_scope_epoch(scope_epoch: u64) -> Result<i64, DbError> {
    i64::try_from(scope_epoch)
        .map_err(|_| DbError::Conflict("PTY epoch exceeds SQLite INTEGER range".into()))
}

fn validate_terminal_claim_token(token: &str) -> Result<(), DbError> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DbError::Conflict(
            "terminal turn admission has an invalid Requirement claim capability".into(),
        ));
    }
    Ok(())
}

fn validate_terminal_turn_scope(scope: &TerminalTurnAdmissionScope) -> Result<i64, DbError> {
    TerminalId::parse(&scope.terminal_id).map_err(|error| {
        DbError::Conflict(format!(
            "terminal turn admission has invalid terminal_id: {error}"
        ))
    })?;
    RequirementId::parse(&scope.requirement_id).map_err(|error| {
        DbError::Conflict(format!(
            "terminal turn admission has invalid requirement_id: {error}"
        ))
    })?;
    if scope.claim_generation < 1 {
        return Err(DbError::Conflict(
            "terminal turn admission claim_generation must be positive".into(),
        ));
    }
    validate_terminal_claim_token(&scope.claim_token)?;
    terminal_turn_scope_epoch(scope.pty_epoch)
}

fn validate_terminal_turn_key(key: &TerminalTurnAdmissionKey) -> Result<i64, DbError> {
    let epoch = validate_terminal_turn_scope(&TerminalTurnAdmissionScope {
        terminal_id: key.terminal_id.clone(),
        pty_epoch: key.pty_epoch,
        requirement_id: key.requirement_id.clone(),
        claim_generation: key.claim_generation,
        claim_token: key.claim_token.clone(),
    })?;
    nomifun_common::validate_uuidv7(&key.turn_token).map_err(|error| {
        DbError::Conflict(format!(
            "terminal turn admission has invalid turn_token: {error}"
        ))
    })?;
    Ok(epoch)
}

async fn fetch_terminal_turn_admission(
    executor: impl sqlx::Executor<'_, Database = Sqlite>,
    key: &TerminalTurnAdmissionKey,
    epoch: i64,
) -> Result<Option<TerminalTurnAdmissionRow>, DbError> {
    Ok(sqlx::query_as::<_, TerminalTurnAdmissionRow>(
        "SELECT * FROM terminal_turn_admissions \
         WHERE terminal_id = ?1 AND pty_epoch = ?2 \
           AND requirement_id = ?3 AND claim_generation = ?4 \
           AND claim_token = ?5 AND turn_token = ?6",
    )
    .bind(&key.terminal_id)
    .bind(epoch)
    .bind(&key.requirement_id)
    .bind(key.claim_generation)
    .bind(&key.claim_token)
    .bind(&key.turn_token)
    .fetch_optional(executor)
    .await?)
}

async fn advance_terminal_turn_phase(
    pool: &SqlitePool,
    key: &TerminalTurnAdmissionKey,
    epoch: i64,
    expected_phase: &str,
    next_phase: &str,
    now: i64,
) -> Result<TerminalTurnEffectsStart, DbError> {
    let mut tx = pool.begin().await?;
    let updated = if next_phase == "effects_started" {
        sqlx::query(
            "UPDATE terminal_turn_admissions \
             SET phase = 'effects_started', effects_started_at = ?1 \
             WHERE terminal_id = ?2 AND pty_epoch = ?3 \
               AND requirement_id = ?4 AND claim_generation = ?5 \
               AND turn_token = ?6 AND phase = ?7 AND claim_token = ?8 \
               AND EXISTS ( \
                   SELECT 1 FROM requirements requirement \
                   WHERE requirement.requirement_id = ?4 \
                     AND requirement.status = 'in_progress' \
                     AND requirement.owner_terminal_id = ?2 \
                     AND requirement.claim_generation = ?5 \
                     AND requirement.claim_token = ?8 \
               ) \
               AND EXISTS ( \
                   SELECT 1 FROM terminal_sessions terminal \
                   WHERE terminal.terminal_id = ?2 \
                     AND terminal.last_status = 'running' \
               )",
        )
        .bind(now)
        .bind(&key.terminal_id)
        .bind(epoch)
        .bind(&key.requirement_id)
        .bind(key.claim_generation)
        .bind(&key.turn_token)
        .bind(expected_phase)
        .bind(&key.claim_token)
        .execute(&mut *tx)
        .await?
    } else {
        debug_assert_eq!(next_phase, "body_written");
        sqlx::query(
            "UPDATE terminal_turn_admissions \
             SET phase = 'body_written' \
             WHERE terminal_id = ?1 AND pty_epoch = ?2 \
               AND requirement_id = ?3 AND claim_generation = ?4 \
               AND turn_token = ?5 AND phase = ?6 AND claim_token = ?7 \
               AND EXISTS ( \
                   SELECT 1 FROM requirements requirement \
                   WHERE requirement.requirement_id = ?3 \
                     AND requirement.status = 'in_progress' \
                     AND requirement.owner_terminal_id = ?1 \
                     AND requirement.claim_generation = ?4 \
                     AND requirement.claim_token = ?7 \
               ) \
               AND EXISTS ( \
                   SELECT 1 FROM terminal_sessions terminal \
                   WHERE terminal.terminal_id = ?1 \
                     AND terminal.last_status = 'running' \
               )",
        )
        .bind(&key.terminal_id)
        .bind(epoch)
        .bind(&key.requirement_id)
        .bind(key.claim_generation)
        .bind(&key.turn_token)
        .bind(expected_phase)
        .bind(&key.claim_token)
        .execute(&mut *tx)
        .await?
    };
    if updated.rows_affected() == 1 {
        tx.commit().await?;
        return Ok(TerminalTurnEffectsStart::Started);
    }

    let row = fetch_terminal_turn_admission(&mut *tx, key, epoch)
        .await?
        .ok_or_else(|| DbError::Conflict("terminal turn admission key does not exist".into()))?;
    match row.phase.as_str() {
        "effects_started" | "body_written" if row.phase != expected_phase => {
            tx.commit().await?;
            return Ok(TerminalTurnEffectsStart::AlreadyStarted);
        }
        "settled" => {
            tx.commit().await?;
            return Ok(TerminalTurnEffectsStart::AlreadySettled);
        }
        phase if phase == expected_phase => {}
        other => {
            return Err(DbError::Init(format!(
                "terminal turn admission has invalid phase {other:?}"
            )));
        }
    }

    // The receipt is still at the expected phase, so the compare-and-set could
    // only have lost its exact Requirement/Terminal authority. Reconcile the
    // receipt inside the same writer transaction and permanently absorb it.
    let requirement_status: Option<(String, i64)> = sqlx::query_as(
        "SELECT status, claim_generation FROM requirements \
         WHERE requirement_id = ?1 AND claim_token = ?2",
    )
    .bind(&key.requirement_id)
    .bind(&key.claim_token)
    .fetch_optional(&mut *tx)
    .await?;
    let outcome = requirement_status
        .as_ref()
        .filter(|(_, generation)| *generation == key.claim_generation)
        .and_then(|(status, _)| match status.as_str() {
            "done" | "failed" | "needs_review" | "cancelled" => Some(status.as_str()),
            _ => None,
        })
        .unwrap_or("needs_review");
    let detail =
        "Terminal turn lost exact Requirement or PTY authority before the next irreversible write; execution was absorbed.";
    sqlx::query(
        "UPDATE terminal_turn_admissions \
         SET phase='settled', outcome=?1, detail=?2, settled_at=?3 \
         WHERE terminal_id=?4 AND pty_epoch=?5 AND requirement_id=?6 \
           AND claim_generation=?7 AND turn_token=?8 AND phase=?9 \
           AND claim_token=?10",
    )
    .bind(outcome)
    .bind(detail)
    .bind(now)
    .bind(&key.terminal_id)
    .bind(epoch)
    .bind(&key.requirement_id)
    .bind(key.claim_generation)
    .bind(&key.turn_token)
    .bind(expected_phase)
    .bind(&key.claim_token)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(TerminalTurnEffectsStart::AlreadySettled)
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
        //
        // A prior process may have crossed the PTY write boundary without
        // persisting the Requirement verdict. Park every such receipt before
        // making the ghost sessions exited. This is intentionally fail-closed:
        // startup recovery must never turn an ambiguous terminal-owned claim
        // back into runnable work.
        let now = now_ms();
        let detail =
            "Terminal process ended during application shutdown or crash recovery; automatic turn outcome is ambiguous and was not executed again.";
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE terminal_turn_admissions \
             SET phase = 'settled', \
                 outcome = COALESCE(( \
                     SELECT CASE requirement.status \
                         WHEN 'done' THEN 'done' \
                         WHEN 'failed' THEN 'failed' \
                         WHEN 'cancelled' THEN 'cancelled' \
                         WHEN 'needs_review' THEN 'needs_review' \
                         ELSE NULL END \
                     FROM requirements requirement \
                     WHERE requirement.requirement_id = terminal_turn_admissions.requirement_id \
                       AND requirement.claim_generation = terminal_turn_admissions.claim_generation \
                       AND requirement.claim_token = terminal_turn_admissions.claim_token \
                 ), 'needs_review'), \
                 detail = ?1, settled_at = ?2 \
             WHERE phase <> 'settled' \
               AND terminal_id IN ( \
                   SELECT terminal_id FROM terminal_sessions \
                   WHERE last_status = 'running' \
               )",
        )
        .bind(detail)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE requirements \
             SET status = 'needs_review', lease_expires_at = NULL, \
                 completion_note = COALESCE(completion_note, ?1), updated_at = ?2 \
             WHERE status = 'in_progress' \
               AND owner_terminal_id IN ( \
                   SELECT terminal_id FROM terminal_sessions \
                   WHERE last_status = 'running' \
               )",
        )
        .bind(detail)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let result = sqlx::query(
            "UPDATE terminal_sessions SET last_status = 'exited', exit_code = NULL, updated_at = ? \
             WHERE last_status = 'running'",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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

    async fn claim_turn_admission(
        &self,
        scope: &TerminalTurnAdmissionScope,
        now: i64,
    ) -> Result<TerminalTurnAdmissionClaim, DbError> {
        let epoch = validate_terminal_turn_scope(scope)?;

        // Fast replay lookup happens outside a transaction. A durable receipt
        // remains absorbing after the Requirement is settled, parked, or its
        // Terminal is deleted.
        if let Some(row) = sqlx::query_as::<_, TerminalTurnAdmissionRow>(
            "SELECT * FROM terminal_turn_admissions \
             WHERE requirement_id = ?1 AND claim_generation = ?2",
        )
        .bind(&scope.requirement_id)
        .bind(scope.claim_generation)
        .fetch_optional(&self.pool)
        .await?
        {
            if row.claim_token.as_deref() != Some(scope.claim_token.as_str()) {
                return Err(DbError::Conflict(
                    "terminal turn receipt belongs to a different Requirement claim capability"
                        .into(),
                ));
            }
            return Ok(TerminalTurnAdmissionClaim {
                row,
                claimed_new: false,
            });
        }

        let mut tx = self.pool.begin().await?;
        // Acquire the SQLite writer before reading again. Starting a deferred
        // transaction with a SELECT lets two concurrent claimants both hold a
        // read snapshot and then race while upgrading; the no-op UPDATE
        // serializes them first on every supported OS/filesystem.
        let terminal_lock = sqlx::query(
            "UPDATE terminal_sessions SET updated_at = updated_at \
             WHERE terminal_id = ?1",
        )
        .bind(&scope.terminal_id)
        .execute(&mut *tx)
        .await?;

        // The writer that waited behind the INSERT winner must observe and
        // absorb that receipt even if the Requirement was already settled or
        // the Terminal was concurrently deleted immediately afterwards.
        if let Some(row) = sqlx::query_as::<_, TerminalTurnAdmissionRow>(
            "SELECT * FROM terminal_turn_admissions \
             WHERE requirement_id = ?1 AND claim_generation = ?2",
        )
        .bind(&scope.requirement_id)
        .bind(scope.claim_generation)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.claim_token.as_deref() != Some(scope.claim_token.as_str()) {
                return Err(DbError::Conflict(
                    "terminal turn receipt belongs to a different Requirement claim capability"
                        .into(),
                ));
            }
            tx.commit().await?;
            return Ok(TerminalTurnAdmissionClaim {
                row,
                claimed_new: false,
            });
        }

        // No receipt exists, so both durable parents must still authorize this
        // exact fresh admission.
        if terminal_lock.rows_affected() != 1 {
            return Err(DbError::Conflict(format!(
                "terminal {} does not exist",
                scope.terminal_id
            )));
        }
        let terminal_running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM terminal_sessions \
             WHERE terminal_id = ?1 AND last_status = 'running'",
        )
        .bind(&scope.terminal_id)
        .fetch_one(&mut *tx)
        .await?;
        if terminal_running != 1 {
            return Err(DbError::Conflict(format!(
                "terminal {} is not durably running",
                scope.terminal_id
            )));
        }
        let requirement = sqlx::query(
            "UPDATE requirements SET updated_at = updated_at \
             WHERE requirement_id = ?1 AND status = 'in_progress' \
               AND owner_terminal_id = ?2 AND claim_generation = ?3 \
               AND claim_token = ?4",
        )
        .bind(&scope.requirement_id)
        .bind(&scope.terminal_id)
        .bind(scope.claim_generation)
        .bind(&scope.claim_token)
        .execute(&mut *tx)
        .await?;
        if requirement.rows_affected() != 1 {
            return Err(DbError::Conflict(format!(
                "requirement {} claim generation {} is not active for terminal {}",
                scope.requirement_id, scope.claim_generation, scope.terminal_id
            )));
        }

        let turn_token = generate_id();
        let inserted = sqlx::query_as::<_, TerminalTurnAdmissionRow>(
            "INSERT INTO terminal_turn_admissions (\
                 turn_token, terminal_id, pty_epoch, requirement_id, \
                 claim_generation, claim_token, phase, outcome, detail, admitted_at, \
                 effects_started_at, settled_at\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'admitted', NULL, NULL, ?7, NULL, NULL) \
             ON CONFLICT(terminal_id, pty_epoch, requirement_id, claim_generation) \
             DO NOTHING \
             RETURNING *",
        )
        .bind(&turn_token)
        .bind(&scope.terminal_id)
        .bind(epoch)
        .bind(&scope.requirement_id)
        .bind(scope.claim_generation)
        .bind(&scope.claim_token)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;

        let (row, claimed_new) = match inserted {
            Some(row) => (row, true),
            None => {
                let row = sqlx::query_as::<_, TerminalTurnAdmissionRow>(
                    "SELECT * FROM terminal_turn_admissions \
                     WHERE terminal_id = ?1 AND pty_epoch = ?2 \
                       AND requirement_id = ?3 AND claim_generation = ?4",
                )
                .bind(&scope.terminal_id)
                .bind(epoch)
                .bind(&scope.requirement_id)
                .bind(scope.claim_generation)
                .fetch_one(&mut *tx)
                .await?;
                if row.claim_token.as_deref() != Some(scope.claim_token.as_str()) {
                    return Err(DbError::Conflict(
                        "terminal turn receipt belongs to a different Requirement claim capability"
                            .into(),
                    ));
                }
                (row, false)
            }
        };
        tx.commit().await?;
        Ok(TerminalTurnAdmissionClaim { row, claimed_new })
    }

    async fn mark_turn_effects_started(
        &self,
        key: &TerminalTurnAdmissionKey,
        now: i64,
    ) -> Result<TerminalTurnEffectsStart, DbError> {
        let epoch = validate_terminal_turn_key(key)?;
        advance_terminal_turn_phase(
            &self.pool,
            key,
            epoch,
            "admitted",
            "effects_started",
            now,
        )
        .await
    }

    async fn mark_turn_body_written(
        &self,
        key: &TerminalTurnAdmissionKey,
        now: i64,
    ) -> Result<TerminalTurnEffectsStart, DbError> {
        let epoch = validate_terminal_turn_key(key)?;
        advance_terminal_turn_phase(
            &self.pool,
            key,
            epoch,
            "admitted",
            "body_written",
            now,
        )
        .await
    }

    async fn mark_turn_submit_started(
        &self,
        key: &TerminalTurnAdmissionKey,
        now: i64,
    ) -> Result<TerminalTurnEffectsStart, DbError> {
        let epoch = validate_terminal_turn_key(key)?;
        advance_terminal_turn_phase(
            &self.pool,
            key,
            epoch,
            "body_written",
            "effects_started",
            now,
        )
        .await
    }

    async fn settle_turn_admission(
        &self,
        key: &TerminalTurnAdmissionKey,
        outcome: TerminalTurnOutcome,
        detail: Option<&str>,
        now: i64,
    ) -> Result<TerminalTurnSettlement, DbError> {
        let epoch = validate_terminal_turn_key(key)?;
        if detail.is_some_and(|value| value.chars().count() > 4000) {
            return Err(DbError::Conflict(
                "terminal turn settlement detail exceeds 4000 characters".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query_as::<_, TerminalTurnAdmissionRow>(
            "UPDATE terminal_turn_admissions \
             SET phase = 'settled', outcome = ?1, detail = ?2, settled_at = ?3 \
             WHERE terminal_id = ?4 AND pty_epoch = ?5 \
               AND requirement_id = ?6 AND claim_generation = ?7 \
               AND turn_token = ?8 AND claim_token = ?9 AND phase <> 'settled' \
               AND EXISTS ( \
                   SELECT 1 FROM requirements requirement \
                   WHERE requirement.requirement_id = ?6 \
                     AND requirement.claim_generation = ?7 \
                     AND requirement.claim_token = ?9 \
                     AND requirement.owner_terminal_id = ?4 \
                     AND requirement.status = ?1 \
               ) \
             RETURNING *",
        )
        .bind(outcome.as_db())
        .bind(detail)
        .bind(now)
        .bind(&key.terminal_id)
        .bind(epoch)
        .bind(&key.requirement_id)
        .bind(key.claim_generation)
        .bind(&key.turn_token)
        .bind(&key.claim_token)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(TerminalTurnSettlement {
                row,
                settled_new: true,
            });
        }
        let row = fetch_terminal_turn_admission(&mut *tx, key, epoch)
            .await?
            .ok_or_else(|| {
                DbError::Conflict("terminal turn admission key does not exist".into())
            })?;
        if row.phase != "settled" {
            return Err(DbError::Conflict(format!(
                "terminal turn outcome {} is not the exact durable Requirement verdict",
                outcome.as_db()
            )));
        }
        if row.outcome.as_deref() != Some(outcome.as_db()) || row.detail.as_deref() != detail {
            return Err(DbError::Conflict(
                "terminal turn admission was already settled with a different outcome".into(),
            ));
        }
        tx.commit().await?;
        Ok(TerminalTurnSettlement {
            row,
            settled_new: false,
        })
    }

    async fn get_turn_admission(
        &self,
        key: &TerminalTurnAdmissionKey,
    ) -> Result<Option<TerminalTurnAdmissionRow>, DbError> {
        let epoch = validate_terminal_turn_key(key)?;
        fetch_terminal_turn_admission(&self.pool, key, epoch).await
    }

    async fn get_turn_admission_for_claim(
        &self,
        terminal_id: &str,
        requirement_id: &str,
        claim_generation: i64,
        claim_token: &str,
    ) -> Result<Option<TerminalTurnAdmissionRow>, DbError> {
        let terminal_id = TerminalId::parse(terminal_id).map_err(|error| {
            DbError::Conflict(format!(
                "terminal turn receipt lookup has invalid terminal_id: {error}"
            ))
        })?;
        let requirement_id = RequirementId::parse(requirement_id).map_err(|error| {
            DbError::Conflict(format!(
                "terminal turn receipt lookup has invalid requirement_id: {error}"
            ))
        })?;
        if claim_generation <= 0 {
            return Err(DbError::Conflict(
                "terminal turn receipt lookup generation must be positive".into(),
            ));
        }
        validate_terminal_claim_token(claim_token)?;
        let row = sqlx::query_as::<_, TerminalTurnAdmissionRow>(
            "SELECT * FROM terminal_turn_admissions \
             WHERE requirement_id=?1 AND claim_generation=?2",
        )
        .bind(requirement_id.as_str())
        .bind(claim_generation)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = &row
            && (row.terminal_id != terminal_id.as_str()
                || row.claim_token.as_deref() != Some(claim_token))
        {
            return Err(DbError::Conflict(
                "terminal turn receipt exists for a different terminal or Requirement claim capability"
                    .into(),
            ));
        }
        Ok(row)
    }

    async fn park_open_turn_admissions(
        &self,
        terminal_id: &str,
        pty_epoch: Option<u64>,
        detail: &str,
        now: i64,
    ) -> Result<u64, DbError> {
        TerminalId::parse(terminal_id).map_err(|error| {
            DbError::Conflict(format!(
                "automatic-turn parking has invalid terminal_id: {error}"
            ))
        })?;
        if detail.trim().is_empty() || detail.chars().count() > 4000 {
            return Err(DbError::Conflict(
                "automatic-turn parking detail must contain 1..=4000 characters".into(),
            ));
        }
        let epoch = pty_epoch.map(terminal_turn_scope_epoch).transpose()?;
        let mut tx = self.pool.begin().await?;
        // Park the still-active exact claims first. The receipt update below
        // then projects the authoritative Requirement status, so a completed
        // Requirement observed at boot remains `done` rather than diverging to
        // a receipt-only `needs_review`.
        match epoch {
            Some(epoch) => {
                sqlx::query(
                    "UPDATE requirements \
                     SET status='needs_review', lease_expires_at=NULL, \
                         completion_note=COALESCE(completion_note, ?1), updated_at=?2 \
                     WHERE status='in_progress' AND owner_terminal_id=?3 \
                       AND EXISTS ( \
                           SELECT 1 FROM terminal_turn_admissions admission \
                           WHERE admission.terminal_id=?3 AND admission.pty_epoch=?4 \
                             AND admission.phase <> 'settled' \
                             AND admission.requirement_id=requirements.requirement_id \
                             AND admission.claim_generation=requirements.claim_generation \
                             AND admission.claim_token=requirements.claim_token \
                       )",
                )
                .bind(detail)
                .bind(now)
                .bind(terminal_id)
                .bind(epoch)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "UPDATE requirements \
                     SET status='needs_review', lease_expires_at=NULL, \
                         completion_note=COALESCE(completion_note, ?1), updated_at=?2 \
                     WHERE status='in_progress' AND owner_terminal_id=?3 \
                       AND EXISTS ( \
                           SELECT 1 FROM terminal_turn_admissions admission \
                           WHERE admission.terminal_id=?3 \
                             AND admission.phase <> 'settled' \
                             AND admission.requirement_id=requirements.requirement_id \
                             AND admission.claim_generation=requirements.claim_generation \
                             AND admission.claim_token=requirements.claim_token \
                       )",
                )
                .bind(detail)
                .bind(now)
                .bind(terminal_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        let parked = match epoch {
            Some(epoch) => {
                sqlx::query(
                    "UPDATE terminal_turn_admissions \
                     SET phase = 'settled', \
                         outcome = COALESCE(( \
                             SELECT CASE requirement.status \
                                 WHEN 'done' THEN 'done' \
                                 WHEN 'failed' THEN 'failed' \
                                 WHEN 'cancelled' THEN 'cancelled' \
                                 WHEN 'needs_review' THEN 'needs_review' \
                                 ELSE NULL END \
                             FROM requirements requirement \
                             WHERE requirement.requirement_id = terminal_turn_admissions.requirement_id \
                               AND requirement.claim_generation = terminal_turn_admissions.claim_generation \
                               AND requirement.claim_token = terminal_turn_admissions.claim_token \
                         ), 'needs_review'), \
                         detail = ?1, settled_at = ?2 \
                     WHERE terminal_id = ?3 AND pty_epoch = ?4 \
                       AND phase <> 'settled'",
                )
                .bind(detail)
                .bind(now)
                .bind(terminal_id)
                .bind(epoch)
                .execute(&mut *tx)
                .await?
            }
            None => {
                sqlx::query(
                    "UPDATE terminal_turn_admissions \
                     SET phase = 'settled', \
                         outcome = COALESCE(( \
                             SELECT CASE requirement.status \
                                 WHEN 'done' THEN 'done' \
                                 WHEN 'failed' THEN 'failed' \
                                 WHEN 'cancelled' THEN 'cancelled' \
                                 WHEN 'needs_review' THEN 'needs_review' \
                                 ELSE NULL END \
                             FROM requirements requirement \
                             WHERE requirement.requirement_id = terminal_turn_admissions.requirement_id \
                               AND requirement.claim_generation = terminal_turn_admissions.claim_generation \
                               AND requirement.claim_token = terminal_turn_admissions.claim_token \
                         ), 'needs_review'), \
                         detail = ?1, settled_at = ?2 \
                     WHERE terminal_id = ?3 AND phase <> 'settled'",
                )
                .bind(detail)
                .bind(now)
                .bind(terminal_id)
                .execute(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(parked.rows_affected())
    }

    async fn delete(&self, id: &str) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let now = now_ms();
        let detail =
            "Terminal was deleted while an automatic turn could be active; outcome is ambiguous and the requirement was not executed again.";
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

        sqlx::query(
            "UPDATE terminal_turn_admissions \
             SET phase = 'settled', \
                 outcome = COALESCE(( \
                     SELECT CASE requirement.status \
                         WHEN 'done' THEN 'done' \
                         WHEN 'failed' THEN 'failed' \
                         WHEN 'cancelled' THEN 'cancelled' \
                         WHEN 'needs_review' THEN 'needs_review' \
                         ELSE NULL END \
                     FROM requirements requirement \
                     WHERE requirement.requirement_id = terminal_turn_admissions.requirement_id \
                       AND requirement.claim_generation = terminal_turn_admissions.claim_generation \
                       AND requirement.claim_token = terminal_turn_admissions.claim_token \
                 ), 'needs_review'), \
                 detail = ?1, settled_at = ?2 \
             WHERE terminal_id = ?3 AND phase <> 'settled'",
        )
        .bind(detail)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
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
             SET status = CASE \
                     WHEN status = 'in_progress' THEN 'needs_review' \
                     ELSE status \
                 END, \
                 completion_note = CASE \
                     WHEN status = 'in_progress' \
                         THEN COALESCE(completion_note, ?1) \
                     ELSE completion_note \
                 END, \
                 lease_expires_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE lease_expires_at \
                 END, \
                 owner_terminal_id = CASE \
                     WHEN status IN ('in_progress', 'needs_review') \
                     THEN owner_terminal_id ELSE NULL END, \
                 updated_at = ?2 \
             WHERE owner_terminal_id = ?3",
        )
        .bind(detail)
        .bind(now)
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
        let now = now_ms();
        let detail =
            "Terminal was removed during application shutdown while an automatic turn could be active; outcome is ambiguous and the requirement was not executed again.";
        sqlx::query(
            "UPDATE terminal_turn_admissions \
             SET phase = 'settled', \
                 outcome = COALESCE(( \
                     SELECT CASE requirement.status \
                         WHEN 'done' THEN 'done' \
                         WHEN 'failed' THEN 'failed' \
                         WHEN 'cancelled' THEN 'cancelled' \
                         WHEN 'needs_review' THEN 'needs_review' \
                         ELSE NULL END \
                     FROM requirements requirement \
                     WHERE requirement.requirement_id = terminal_turn_admissions.requirement_id \
                       AND requirement.claim_generation = terminal_turn_admissions.claim_generation \
                       AND requirement.claim_token = terminal_turn_admissions.claim_token \
                 ), 'needs_review'), \
                 detail = ?1, settled_at = ?2 \
             WHERE phase <> 'settled'",
        )
        .bind(detail)
        .bind(now)
        .execute(&mut *tx)
        .await?;
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
             SET status = CASE \
                     WHEN status = 'in_progress' THEN 'needs_review' \
                     ELSE status \
                 END, \
                 completion_note = CASE \
                     WHEN status = 'in_progress' \
                         THEN COALESCE(completion_note, ?1) \
                     ELSE completion_note \
                 END, \
                 lease_expires_at = CASE \
                     WHEN status = 'in_progress' THEN NULL \
                     ELSE lease_expires_at \
                 END, \
                 owner_terminal_id = CASE \
                     WHEN status IN ('in_progress', 'needs_review') \
                     THEN owner_terminal_id ELSE NULL END, \
                 updated_at = ?2 \
             WHERE owner_terminal_id IS NOT NULL",
        )
        .bind(detail)
        .bind(now)
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
    use std::sync::Arc;

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

    async fn insert_active_requirement(
        db: &crate::Database,
        terminal_id: &TerminalId,
    ) -> RequirementId {
        let requirement_id = RequirementId::new();
        let display_no: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(display_no), 0) + 1 FROM requirements")
                .fetch_one(db.pool())
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO requirements (\
                 requirement_id, display_no, title, tag, status, \
                 attempt_count, created_at, updated_at, claim_generation\
             ) VALUES (?1, ?2, 'terminal admission test', 'test', 'pending', \
                       0, 10, 10, 0)",
        )
        .bind(requirement_id.as_str())
        .bind(display_no)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "UPDATE requirements \
             SET status='in_progress', owner_terminal_id=?1, \
                 active_turn_started_at=10, lease_expires_at=10000, \
                 started_at=10, attempt_count=1, claim_generation=1, \
                 claim_token=?2 \
             WHERE requirement_id=?3",
        )
        .bind(terminal_id.as_str())
        .bind("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .bind(requirement_id.as_str())
        .execute(db.pool())
        .await
        .unwrap();
        requirement_id
    }

    async fn admission_fixture(
        db: &crate::Database,
        repo: &SqliteTerminalRepository,
        owner: &str,
        pty_epoch: u64,
    ) -> (TerminalId, RequirementId, TerminalTurnAdmissionScope) {
        let terminal_id = repo.create(&params(owner)).await.unwrap().terminal_id;
        let requirement_id = insert_active_requirement(db, &terminal_id).await;
        let scope = TerminalTurnAdmissionScope {
            terminal_id: terminal_id.to_string(),
            pty_epoch,
            requirement_id: requirement_id.to_string(),
            claim_generation: 1,
            claim_token:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
        };
        (terminal_id, requirement_id, scope)
    }

    #[tokio::test]
    async fn concurrent_turn_admission_has_exactly_one_insert_winner() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (_, _, scope) = admission_fixture(&db, &repo, &owner, 7).await;
        let first_repo = repo.clone();
        let second_repo = repo.clone();
        let first_scope = scope.clone();
        let second_scope = scope.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let first_barrier = barrier.clone();
        let second_barrier = barrier.clone();

        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repo.claim_turn_admission(&first_scope, 100).await
        });
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repo.claim_turn_admission(&second_scope, 101).await
        });
        barrier.wait().await;
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();

        assert_ne!(first.claimed_new, second.claimed_new);
        assert_eq!(first.row.turn_token, second.row.turn_token);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM terminal_turn_admissions")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn turn_admission_phases_are_absorbing_and_key_drift_fails_closed() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (_, requirement_id, scope) = admission_fixture(&db, &repo, &owner, 11).await;
        let claim = repo.claim_turn_admission(&scope, 100).await.unwrap();
        assert!(claim.claimed_new);
        let key = TerminalTurnAdmissionKey::from_row(&claim.row).unwrap();

        let replay = repo.claim_turn_admission(&scope, 101).await.unwrap();
        assert!(!replay.claimed_new);
        assert_eq!(replay.row.turn_token, key.turn_token);
        assert_eq!(
            repo.mark_turn_effects_started(&key, 102).await.unwrap(),
            TerminalTurnEffectsStart::Started
        );
        assert_eq!(
            repo.mark_turn_effects_started(&key, 103).await.unwrap(),
            TerminalTurnEffectsStart::AlreadyStarted
        );

        let mut wrong_token = key.clone();
        wrong_token.turn_token = generate_id();
        assert!(matches!(
            repo.mark_turn_effects_started(&wrong_token, 104)
                .await
                .unwrap_err(),
            DbError::Conflict(_)
        ));
        let mut wrong_epoch = key.clone();
        wrong_epoch.pty_epoch += 1;
        assert!(matches!(
            repo.mark_turn_effects_started(&wrong_epoch, 104)
                .await
                .unwrap_err(),
            DbError::Conflict(_)
        ));

        assert!(matches!(
            repo.settle_turn_admission(
                &key,
                TerminalTurnOutcome::Done,
                Some("durable verdict"),
                105,
            )
            .await
            .unwrap_err(),
            DbError::Conflict(_)
        ));
        sqlx::query("UPDATE requirements SET status = 'done' WHERE requirement_id = ?")
            .bind(requirement_id.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        let settled = repo
            .settle_turn_admission(
                &key,
                TerminalTurnOutcome::Done,
                Some("durable verdict"),
                106,
            )
            .await
            .unwrap();
        assert!(settled.settled_new);
        assert_eq!(
            repo.mark_turn_effects_started(&key, 107).await.unwrap(),
            TerminalTurnEffectsStart::AlreadySettled
        );
        assert!(
            !repo
                .settle_turn_admission(
                    &key,
                    TerminalTurnOutcome::Done,
                    Some("durable verdict"),
                    108,
                )
                .await
                .unwrap()
                .settled_new
        );
        assert!(matches!(
            repo.settle_turn_admission(
                &key,
                TerminalTurnOutcome::Failed,
                Some("different"),
                109,
            )
            .await
            .unwrap_err(),
            DbError::Conflict(_)
        ));
        let permanent_replay = repo.claim_turn_admission(&scope, 110).await.unwrap();
        assert!(!permanent_replay.claimed_new);
        assert_eq!(permanent_replay.row.outcome.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn effects_and_two_part_submit_revalidate_the_exact_requirement_claim() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());

        let (_, one_step_requirement, one_step_scope) =
            admission_fixture(&db, &repo, &owner, 15).await;
        let one_step = repo.claim_turn_admission(&one_step_scope, 100).await.unwrap();
        let one_step_key = TerminalTurnAdmissionKey::from_row(&one_step.row).unwrap();
        sqlx::query("UPDATE requirements SET status='done' WHERE requirement_id=?")
            .bind(one_step_requirement.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(
            repo.mark_turn_effects_started(&one_step_key, 101)
                .await
                .unwrap(),
            TerminalTurnEffectsStart::AlreadySettled,
            "a verdict committed after admission must prevent the PTY effects boundary"
        );
        let one_step_receipt = repo
            .get_turn_admission(&one_step_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(one_step_receipt.outcome.as_deref(), Some("done"));

        let (_, split_requirement, split_scope) =
            admission_fixture(&db, &repo, &owner, 16).await;
        let split = repo.claim_turn_admission(&split_scope, 200).await.unwrap();
        let split_key = TerminalTurnAdmissionKey::from_row(&split.row).unwrap();
        assert_eq!(
            repo.mark_turn_body_written(&split_key, 201)
                .await
                .unwrap(),
            TerminalTurnEffectsStart::Started
        );
        sqlx::query("UPDATE requirements SET status='cancelled' WHERE requirement_id=?")
            .bind(split_requirement.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(
            repo.mark_turn_submit_started(&split_key, 202)
                .await
                .unwrap(),
            TerminalTurnEffectsStart::AlreadySettled,
            "the delayed submit key must be suppressed after cancellation"
        );
        let split_receipt = repo
            .get_turn_admission(&split_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(split_receipt.phase, "settled");
        assert_eq!(split_receipt.outcome.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn wrong_claim_capability_never_looks_unadmitted_or_requeues_terminal_work() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (terminal_id, requirement_id, scope) =
            admission_fixture(&db, &repo, &owner, 18).await;
        let claim = repo.claim_turn_admission(&scope, 100).await.unwrap();
        let key = TerminalTurnAdmissionKey::from_row(&claim.row).unwrap();
        let mut wrong_key = key.clone();
        wrong_key.claim_token =
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_owned();

        assert!(matches!(
            repo.get_turn_admission_for_claim(
                terminal_id.as_str(),
                requirement_id.as_str(),
                key.claim_generation,
                &wrong_key.claim_token,
            )
            .await
            .unwrap_err(),
            DbError::Conflict(_)
        ));
        assert!(matches!(
            repo.mark_turn_effects_started(&wrong_key, 101)
                .await
                .unwrap_err(),
            DbError::Conflict(_)
        ));

        assert_eq!(
            repo.park_open_turn_admissions(
                terminal_id.as_str(),
                Some(key.pty_epoch),
                "wrong capability cleanup failed closed",
                102,
            )
            .await
            .unwrap(),
            1
        );
        let requirement: (String, Option<String>, i64) = sqlx::query_as(
            "SELECT status, claim_token, claim_generation \
             FROM requirements WHERE requirement_id=?",
        )
        .bind(requirement_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(requirement.0, "needs_review");
        assert_eq!(requirement.1.as_deref(), Some(key.claim_token.as_str()));
        assert_eq!(requirement.2, key.claim_generation);
        let pending_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM requirements \
             WHERE requirement_id=? AND status='pending'",
        )
        .bind(requirement_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            pending_count, 0,
            "capability mismatch cleanup must never requeue admitted work"
        );
    }

    #[tokio::test]
    async fn different_epoch_cannot_readmit_the_same_requirement_claim() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (_, _, scope) = admission_fixture(&db, &repo, &owner, 21).await;
        let first = repo.claim_turn_admission(&scope, 100).await.unwrap();
        assert!(first.claimed_new);
        let mut changed_epoch = scope;
        changed_epoch.pty_epoch = 22;
        let replay = repo
            .claim_turn_admission(&changed_epoch, 101)
            .await
            .unwrap();
        assert!(!replay.claimed_new);
        assert_eq!(replay.row.turn_token, first.row.turn_token);
        assert_eq!(replay.row.pty_epoch, 21);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM terminal_turn_admissions \
                 WHERE requirement_id = ? AND claim_generation = 1",
            )
            .bind(&changed_epoch.requirement_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
            1,
            "one Requirement claim generation must never gain a second PTY execution right"
        );
    }

    #[tokio::test]
    async fn exact_epoch_parking_and_delete_preserve_absorbing_receipt() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (terminal_id, requirement_id, scope) =
            admission_fixture(&db, &repo, &owner, 31).await;
        let claim = repo.claim_turn_admission(&scope, 100).await.unwrap();
        let key = TerminalTurnAdmissionKey::from_row(&claim.row).unwrap();

        assert_eq!(
            repo.park_open_turn_admissions(
                terminal_id.as_str(),
                Some(30),
                "old epoch",
                101,
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            repo.get_turn_admission(&key)
                .await
                .unwrap()
                .unwrap()
                .phase,
            "admitted"
        );
        assert_eq!(
            repo.park_open_turn_admissions(
                terminal_id.as_str(),
                Some(31),
                "exact epoch ended",
                102,
            )
            .await
            .unwrap(),
            1
        );
        let requirement: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT status, lease_expires_at, claim_generation \
             FROM requirements WHERE requirement_id = ?",
        )
        .bind(requirement_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(requirement.0, "needs_review");
        assert_eq!(requirement.1, None);
        assert_eq!(requirement.2, 1);

        repo.delete(terminal_id.as_str()).await.unwrap();
        let receipt = repo.get_turn_admission(&key).await.unwrap().unwrap();
        assert_eq!(receipt.phase, "settled");
        assert_eq!(receipt.outcome.as_deref(), Some("needs_review"));
        assert!(matches!(
            repo.mark_turn_effects_started(&key, 103).await.unwrap(),
            TerminalTurnEffectsStart::AlreadySettled
        ));
    }

    #[tokio::test]
    async fn boot_reconciliation_parks_open_effects_and_exact_requirement_claim() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (terminal_id, requirement_id, scope) =
            admission_fixture(&db, &repo, &owner, 41).await;
        let claim = repo.claim_turn_admission(&scope, 100).await.unwrap();
        let key = TerminalTurnAdmissionKey::from_row(&claim.row).unwrap();
        assert_eq!(
            repo.mark_turn_effects_started(&key, 101).await.unwrap(),
            TerminalTurnEffectsStart::Started
        );

        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 1);
        let receipt = repo.get_turn_admission(&key).await.unwrap().unwrap();
        assert_eq!(receipt.phase, "settled");
        assert_eq!(receipt.outcome.as_deref(), Some("needs_review"));
        let requirement: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT status, lease_expires_at, claim_generation \
             FROM requirements WHERE requirement_id = ?",
        )
        .bind(requirement_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(requirement, ("needs_review".into(), None, 1));
        assert_eq!(
            repo.get_by_id(terminal_id.as_str())
                .await
                .unwrap()
                .unwrap()
                .last_status,
            "exited"
        );
    }

    #[tokio::test]
    async fn boot_reconciliation_projects_a_preexisting_done_verdict_into_the_receipt() {
        let db = init_database_memory().await.unwrap();
        let owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteTerminalRepository::new(db.pool().clone());
        let (_, requirement_id, scope) =
            admission_fixture(&db, &repo, &owner, 42).await;
        let claim = repo.claim_turn_admission(&scope, 100).await.unwrap();
        let key = TerminalTurnAdmissionKey::from_row(&claim.row).unwrap();
        assert_eq!(
            repo.mark_turn_effects_started(&key, 101).await.unwrap(),
            TerminalTurnEffectsStart::Started
        );
        sqlx::query("UPDATE requirements SET status='done' WHERE requirement_id=?")
            .bind(requirement_id.as_str())
            .execute(db.pool())
            .await
            .unwrap();

        assert_eq!(repo.mark_all_running_exited().await.unwrap(), 1);
        let receipt = repo.get_turn_admission(&key).await.unwrap().unwrap();
        assert_eq!(receipt.phase, "settled");
        assert_eq!(receipt.outcome.as_deref(), Some("done"));
        let status: String =
            sqlx::query_scalar("SELECT status FROM requirements WHERE requirement_id=?")
                .bind(requirement_id.as_str())
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(status, "done");
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
