use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{
    IdmmActionReservationRow, IdmmInterventionRow, NewIdmmInterventionRow,
};
use crate::repository::idmm_intervention::{
    IIdmmInterventionRepository, IdmmActionReservationKey, IdmmActionReserveResult,
    IdmmActionSettleResult, IdmmActionSettlement, IdmmActionTurnIdentity,
    MAX_IDMM_ACTION_FAILURE_REASON_CHARS, PER_TARGET_CAP, ReserveIdmmActionParams,
};

#[derive(Clone, Debug)]
pub struct SqliteIdmmInterventionRepository {
    pool: SqlitePool,
}

impl SqliteIdmmInterventionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn checked_turn_generation(generation: u64) -> Result<i64, DbError> {
    i64::try_from(generation).map_err(|_| {
        DbError::Conflict(format!(
            "IDMM turn generation {generation} exceeds SQLite's signed integer range"
        ))
    })
}

fn validate_action_identity(action_identity: &str) -> Result<(), DbError> {
    if action_identity.len() != 64
        || !action_identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DbError::Conflict(
            "IDMM action identity must be canonical lowercase SHA-256 hex".to_owned(),
        ));
    }
    Ok(())
}

fn validate_failure_reason(reason: &str) -> Result<(), DbError> {
    if reason.trim().is_empty() {
        return Err(DbError::Conflict(
            "IDMM action failure reason must not be empty".to_owned(),
        ));
    }
    if reason.chars().count() > MAX_IDMM_ACTION_FAILURE_REASON_CHARS {
        return Err(DbError::Conflict(format!(
            "IDMM action failure reason exceeds {MAX_IDMM_ACTION_FAILURE_REASON_CHARS} characters"
        )));
    }
    Ok(())
}

fn validate_action_key(key: &IdmmActionReservationKey) -> Result<i64, DbError> {
    nomifun_common::UserId::parse(&key.user_id).map_err(|error| {
        DbError::Conflict(format!(
            "IDMM action owner '{}' is not a canonical UUIDv7: {error}",
            key.user_id
        ))
    })?;
    nomifun_common::ConversationId::parse(&key.conversation_id).map_err(|error| {
        DbError::Conflict(format!(
            "IDMM action conversation '{}' is not a canonical UUIDv7: {error}",
            key.conversation_id
        ))
    })?;
    nomifun_common::MessageId::parse(&key.turn_id).map_err(|error| {
        DbError::Conflict(format!(
            "IDMM action turn '{}' is not a canonical UUIDv7: {error}",
            key.turn_id
        ))
    })?;
    validate_action_identity(&key.action_identity)?;
    checked_turn_generation(key.turn_generation)
}

fn validate_turn_identity(turn: &IdmmActionTurnIdentity) -> Result<i64, DbError> {
    nomifun_common::UserId::parse(&turn.user_id).map_err(|error| {
        DbError::Conflict(format!(
            "IDMM action owner '{}' is not a canonical UUIDv7: {error}",
            turn.user_id
        ))
    })?;
    nomifun_common::ConversationId::parse(&turn.conversation_id).map_err(|error| {
        DbError::Conflict(format!(
            "IDMM action conversation '{}' is not a canonical UUIDv7: {error}",
            turn.conversation_id
        ))
    })?;
    nomifun_common::MessageId::parse(&turn.turn_id).map_err(|error| {
        DbError::Conflict(format!(
            "IDMM action turn '{}' is not a canonical UUIDv7: {error}",
            turn.turn_id
        ))
    })?;
    checked_turn_generation(turn.turn_generation)
}

async fn lock_conversation_owner(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    conversation_id: &str,
) -> Result<String, DbError> {
    // The no-op update obtains SQLite's write lock before any reservation
    // lookup/insert, linearizing action admission against terminal status and
    // Conversation deletion.
    let locked = sqlx::query(
        "UPDATE conversations SET updated_at = updated_at \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(conversation_id)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;
    if locked.rows_affected() == 0 {
        return Err(DbError::Conflict(
            "IDMM action conversation target owner mismatch or target missing".to_owned(),
        ));
    }
    sqlx::query_scalar("SELECT status FROM conversations WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(DbError::Query)
}

async fn load_action_reservation(
    transaction: &mut Transaction<'_, Sqlite>,
    key: &IdmmActionReservationKey,
    turn_generation: i64,
) -> Result<Option<IdmmActionReservationRow>, DbError> {
    sqlx::query_as::<_, IdmmActionReservationRow>(
        "SELECT * FROM idmm_action_reservations \
         WHERE conversation_id = ? AND turn_id = ? \
           AND turn_generation = ? AND action_identity = ?",
    )
    .bind(&key.conversation_id)
    .bind(&key.turn_id)
    .bind(turn_generation)
    .bind(&key.action_identity)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Query)
}

fn classify_duplicate(
    row: IdmmActionReservationRow,
) -> Result<IdmmActionReserveResult, DbError> {
    match row.status.as_str() {
        "reserved" => Ok(IdmmActionReserveResult::AlreadyReserved(row)),
        "applied" | "failed" => Ok(IdmmActionReserveResult::Completed(row)),
        status => Err(DbError::Init(format!(
            "IDMM action reservation has invalid status '{status}'"
        ))),
    }
}

#[async_trait::async_trait]
impl IIdmmInterventionRepository for SqliteIdmmInterventionRepository {
    async fn insert(&self, row: &NewIdmmInterventionRow) -> Result<IdmmInterventionRow, DbError> {
        let mut transaction = self.pool.begin().await?;
        let intervention_id =
            nomifun_common::IdmmInterventionId::parse(&row.intervention_id).map_err(|error| {
                DbError::Conflict(format!(
                    "IDMM intervention_id '{}' is not a canonical UUIDv7: {error}",
                    row.intervention_id
                ))
            })?;
        match row.target_kind.as_str() {
            "conversation" => {
                let target = nomifun_common::ConversationId::parse(&row.target_id).map_err(|error| {
                    DbError::Conflict(format!(
                        "IDMM conversation target '{}' is not a canonical UUIDv7: {error}",
                        row.target_id
                    ))
                })?;
                let locked = sqlx::query(
                    "UPDATE conversations SET updated_at = updated_at \
                     WHERE conversation_id = ? AND user_id = ?",
                )
                .bind(target.as_str())
                .bind(&row.user_id)
                .execute(&mut *transaction)
                .await?;
                if locked.rows_affected() == 0 {
                    return Err(DbError::Conflict(
                        "IDMM conversation target owner mismatch".into(),
                    ));
                }
            }
            "terminal" => {
                let target = nomifun_common::TerminalId::parse(&row.target_id).map_err(|error| {
                    DbError::Conflict(format!(
                        "IDMM terminal target '{}' is not a canonical UUIDv7: {error}",
                        row.target_id
                    ))
                })?;
                let locked = sqlx::query(
                    "UPDATE terminal_sessions SET updated_at = updated_at \
                     WHERE terminal_id = ? AND user_id = ?",
                )
                .bind(target.as_str())
                .bind(&row.user_id)
                .execute(&mut *transaction)
                .await?;
                if locked.rows_affected() == 0 {
                    return Err(DbError::Conflict(
                        "IDMM terminal target owner mismatch".into(),
                    ));
                }
            }
            _ => {
                return Err(DbError::Conflict(format!(
                    "unsupported IDMM target kind '{}'",
                    row.target_kind
                )));
            }
        }
        let inserted = sqlx::query_as::<_, IdmmInterventionRow>(
            "INSERT INTO idmm_interventions (\
                intervention_id, user_id, target_kind, target_id, watch, at, signal, tier_used, category, \
                action, detail, reason, confidence, bypass_model, outcome\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
            RETURNING *",
        )
        .bind(intervention_id.as_str())
        .bind(&row.user_id)
        .bind(&row.target_kind)
        .bind(&row.target_id)
        .bind(&row.watch)
        .bind(row.at)
        .bind(&row.signal)
        .bind(&row.tier_used)
        .bind(&row.category)
        .bind(&row.action)
        .bind(&row.detail)
        .bind(&row.reason)
        .bind(row.confidence)
        .bind(&row.bypass_model)
        .bind(&row.outcome)
        .fetch_one(&mut *transaction)
        .await?;

        // 激进淘汰:每写入即把该 target 裁到最近 PER_TARGET_CAP 条(数据可丢)。
        sqlx::query(
            "DELETE FROM idmm_interventions \
              WHERE user_id = ?1 AND target_kind = ?2 AND target_id = ?3 \
                AND id NOT IN (\
                  SELECT id FROM idmm_interventions \
                   WHERE user_id = ?1 AND target_kind = ?2 AND target_id = ?3 \
                   ORDER BY at DESC, id DESC LIMIT ?4\
                )",
        )
        .bind(&row.user_id)
        .bind(&row.target_kind)
        .bind(&row.target_id)
        .bind(PER_TARGET_CAP)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        Ok(inserted)
    }

    async fn list_for_target(
        &self,
        user_id: &str,
        target_kind: &str,
        target_id: &str,
        limit: i64,
    ) -> Result<Vec<IdmmInterventionRow>, DbError> {
        let rows = sqlx::query_as::<_, IdmmInterventionRow>(
            "SELECT * FROM idmm_interventions \
              WHERE user_id = ? AND target_kind = ? AND target_id = ? \
              ORDER BY at DESC, id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(target_kind)
        .bind(target_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete_for_target(
        &self,
        user_id: &str,
        target_kind: &str,
        target_id: &str,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM idmm_interventions WHERE user_id = ? AND target_kind = ? AND target_id = ?",
        )
            .bind(user_id)
            .bind(target_kind)
            .bind(target_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn list_recent(&self, user_id: &str, limit: i64) -> Result<Vec<IdmmInterventionRow>, DbError> {
        let rows = sqlx::query_as::<_, IdmmInterventionRow>(
            "SELECT * FROM idmm_interventions WHERE user_id = ? ORDER BY at DESC, id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn clear_all(&self, user_id: &str) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM idmm_interventions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn sweep_all_owners(&self, cutoff_ms: i64, per_user_cap: i64) -> Result<u64, DbError> {
        // 先按 TTL 删旧。
        let by_ttl = sqlx::query("DELETE FROM idmm_interventions WHERE at < ?")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?
            .rows_affected();

        // Apply the hard cap independently per owner. One busy account must
        // not evict another account's activity history.
        let by_cap = sqlx::query(
            "DELETE FROM idmm_interventions \
              WHERE id IN (\
                SELECT id FROM (\
                  SELECT id, ROW_NUMBER() OVER (\
                    PARTITION BY user_id ORDER BY at DESC, id DESC\
                  ) AS owner_rank \
                  FROM idmm_interventions\
                ) WHERE owner_rank > ?\
              )",
        )
        .bind(per_user_cap.max(1))
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(by_ttl + by_cap)
    }

    async fn reserve_action(
        &self,
        params: &ReserveIdmmActionParams,
    ) -> Result<IdmmActionReserveResult, DbError> {
        let turn_generation = validate_action_key(&params.key)?;
        let mut transaction = self.pool.begin().await?;
        let conversation_status = lock_conversation_owner(
            &mut transaction,
            &params.key.user_id,
            &params.key.conversation_id,
        )
        .await?;

        // Look up first while holding the Conversation/write lock. A replay
        // after terminal completion must still return the durable absorbing
        // result; only a genuinely new action requires Running authority.
        if let Some(existing) =
            load_action_reservation(&mut transaction, &params.key, turn_generation).await?
        {
            transaction.commit().await?;
            return classify_duplicate(existing);
        }
        if conversation_status != "running" {
            return Err(DbError::Conflict(format!(
                "IDMM action reservation requires a Running Conversation, found '{conversation_status}'"
            )));
        }

        let reservation_id = nomifun_common::generate_id();
        let inserted = sqlx::query(
            "INSERT INTO idmm_action_reservations (\
                 reservation_id, user_id, conversation_id, turn_id, \
                 turn_generation, action_identity, status, reserved_at\
             ) VALUES (?, ?, ?, ?, ?, ?, 'reserved', ?) \
             ON CONFLICT(conversation_id, turn_id, turn_generation, action_identity) DO NOTHING",
        )
        .bind(&reservation_id)
        .bind(&params.key.user_id)
        .bind(&params.key.conversation_id)
        .bind(&params.key.turn_id)
        .bind(turn_generation)
        .bind(&params.key.action_identity)
        .bind(params.reserved_at)
        .execute(&mut *transaction)
        .await?;

        let row = load_action_reservation(&mut transaction, &params.key, turn_generation)
            .await?
            .ok_or_else(|| {
                DbError::Init(
                    "IDMM action reservation insert completed without a durable row".to_owned(),
                )
            })?;
        transaction.commit().await?;
        if inserted.rows_affected() == 1 {
            if row.status != "reserved" {
                return Err(DbError::Init(format!(
                    "new IDMM action reservation has invalid status '{}'",
                    row.status
                )));
            }
            Ok(IdmmActionReserveResult::Reserved(row))
        } else {
            classify_duplicate(row)
        }
    }

    async fn settle_action(
        &self,
        key: &IdmmActionReservationKey,
        settlement: &IdmmActionSettlement,
        settled_at: i64,
    ) -> Result<IdmmActionSettleResult, DbError> {
        let turn_generation = validate_action_key(key)?;
        let (status, source, reason): (&str, &str, Option<&str>) = match settlement {
            IdmmActionSettlement::Applied => ("applied", "execution", None),
            IdmmActionSettlement::Failed { reason } => {
                validate_failure_reason(reason)?;
                ("failed", "execution", Some(reason.as_str()))
            }
            IdmmActionSettlement::Recovered { reason } => {
                validate_failure_reason(reason)?;
                ("failed", "recovery", Some(reason.as_str()))
            }
        };

        let mut transaction = self.pool.begin().await?;
        lock_conversation_owner(&mut transaction, &key.user_id, &key.conversation_id).await?;
        let current = load_action_reservation(&mut transaction, key, turn_generation)
            .await?
            .ok_or_else(|| {
                DbError::NotFound(format!(
                    "IDMM action reservation for turn {} generation {} action {}",
                    key.turn_id, key.turn_generation, key.action_identity
                ))
            })?;
        if current.status != "reserved" {
            transaction.commit().await?;
            return Ok(IdmmActionSettleResult::AlreadySettled(current));
        }

        let settled = sqlx::query_as::<_, IdmmActionReservationRow>(
            "UPDATE idmm_action_reservations \
             SET status = ?, settlement_source = ?, failure_reason = ?, settled_at = ? \
             WHERE id = ? AND status = 'reserved' \
             RETURNING *",
        )
        .bind(status)
        .bind(source)
        .bind(reason)
        .bind(settled_at)
        .bind(current.id)
        .fetch_optional(&mut *transaction)
        .await?;
        let result = if let Some(settled) = settled {
            IdmmActionSettleResult::Settled(settled)
        } else {
            let row = load_action_reservation(&mut transaction, key, turn_generation)
                .await?
                .ok_or_else(|| {
                    DbError::Init(
                        "IDMM action reservation disappeared during settlement".to_owned(),
                    )
                })?;
            IdmmActionSettleResult::AlreadySettled(row)
        };
        transaction.commit().await?;
        Ok(result)
    }

    async fn list_reserved_actions_for_turn(
        &self,
        turn: &IdmmActionTurnIdentity,
    ) -> Result<Vec<IdmmActionReservationRow>, DbError> {
        let turn_generation = validate_turn_identity(turn)?;
        let target_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                 SELECT 1 FROM conversations \
                 WHERE conversation_id = ? AND user_id = ?\
             )",
        )
        .bind(&turn.conversation_id)
        .bind(&turn.user_id)
        .fetch_one(&self.pool)
        .await?;
        if !target_exists {
            return Err(DbError::Conflict(
                "IDMM action conversation target owner mismatch or target missing".to_owned(),
            ));
        }
        sqlx::query_as::<_, IdmmActionReservationRow>(
            "SELECT * FROM idmm_action_reservations \
             WHERE user_id = ? AND conversation_id = ? AND turn_id = ? \
               AND turn_generation = ? AND status = 'reserved' \
             ORDER BY id",
        )
        .bind(&turn.user_id)
        .bind(&turn.conversation_id)
        .bind(&turn.turn_id)
        .bind(turn_generation)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::Query)
    }

    async fn recover_reserved_actions_for_turn(
        &self,
        turn: &IdmmActionTurnIdentity,
        reason: &str,
        settled_at: i64,
    ) -> Result<Vec<IdmmActionReservationRow>, DbError> {
        let turn_generation = validate_turn_identity(turn)?;
        validate_failure_reason(reason)?;
        let mut transaction = self.pool.begin().await?;
        lock_conversation_owner(&mut transaction, &turn.user_id, &turn.conversation_id).await?;
        let mut recovered = sqlx::query_as::<_, IdmmActionReservationRow>(
            "UPDATE idmm_action_reservations \
             SET status = 'failed', settlement_source = 'recovery', \
                 failure_reason = ?, settled_at = ? \
             WHERE user_id = ? AND conversation_id = ? AND turn_id = ? \
               AND turn_generation = ? AND status = 'reserved' \
             RETURNING *",
        )
        .bind(reason)
        .bind(settled_at)
        .bind(&turn.user_id)
        .bind(&turn.conversation_id)
        .bind(&turn.turn_id)
        .bind(turn_generation)
        .fetch_all(&mut *transaction)
        .await?;
        recovered.sort_by_key(|row| row.id);
        transaction.commit().await?;
        Ok(recovered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const CONVERSATION_A: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const CONVERSATION_B: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
    const OWNER_B_CONVERSATION: &str = "0190f5fe-7c00-7a00-8abc-012345678903";
    const TERMINAL_A: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const OWNER_B: &str = "0190f5fe-7c00-7a00-8abc-012345678904";

    async fn setup() -> (SqliteIdmmInterventionRepository, crate::Database, String) {
        let db = init_database_memory().await.unwrap();
        let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
        for (id, name) in [(CONVERSATION_A, "conversation-a"), (CONVERSATION_B, "conversation-b")] {
            sqlx::query(
                "INSERT INTO conversations \
                 (conversation_id, user_id, name, type, extra, status, created_at, updated_at) \
                 VALUES (?, ?, ?, 'nomi', '{}', 'pending', 1, 1)",
            )
            .bind(id)
            .bind(&installation_owner)
            .bind(name)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO terminal_sessions \
             (terminal_id, name, cwd, command, args, created_at, updated_at, user_id) \
             VALUES (?, 'terminal-a', '/tmp', '$SHELL', '[]', 1, 1, ?)",
        )
        .bind(TERMINAL_A)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteIdmmInterventionRepository::new(db.pool().clone());
        (repo, db, installation_owner)
    }

    async fn insert_user(db: &crate::Database, id: &str) {
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
             VALUES (?, ?, 'hash', 1, 1)",
        )
        .bind(id)
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations \
             (conversation_id, user_id, name, type, extra, status, delegation_policy, created_at, updated_at) \
             VALUES (?, ?, 'owner-b-conversation', 'nomi', '{}', 'pending', 'disabled', 1, 1)",
        )
        .bind(OWNER_B_CONVERSATION)
        .bind(id)
        .execute(db.pool())
        .await
            .unwrap();
    }

    async fn set_conversation_status(db: &crate::Database, conversation_id: &str, status: &str) {
        let mut tx = db.pool().begin().await.unwrap();
        match status {
            "running" => {
                let owner: String =
                    sqlx::query_scalar("SELECT user_id FROM conversations WHERE conversation_id = ?")
                        .bind(conversation_id)
                        .fetch_one(&mut *tx)
                        .await
                        .unwrap();
                let operation_id = format!("idmm-test-turn:{conversation_id}");
                sqlx::query(
                    "INSERT INTO conversation_delivery_receipts (\
                        operation_id, message_id, conversation_id, projected_conversation_id, \
                        projected_message_id, user_id, kind, request_payload, status, created_at, updated_at\
                     ) VALUES (?, ?, ?, ?, NULL, ?, 'turn', '{}', 'accepted', 1, 1)",
                )
                .bind(&operation_id)
                .bind(nomifun_common::MessageId::new().as_str())
                .bind(conversation_id)
                .bind(conversation_id)
                .bind(owner)
                .execute(&mut *tx)
                .await
                .unwrap();
                sqlx::query(
                    "UPDATE conversations \
                     SET status = 'running', active_turn_operation_id = ?, \
                         admission_epoch = admission_epoch + 1 \
                     WHERE conversation_id = ?",
                )
                .bind(operation_id)
                .bind(conversation_id)
                .execute(&mut *tx)
                .await
                .unwrap();
            }
            "finished" => {
                let operation_id: String = sqlx::query_scalar(
                    "SELECT active_turn_operation_id FROM conversations \
                     WHERE conversation_id = ?",
                )
                .bind(conversation_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
                sqlx::query(
                    "UPDATE conversation_delivery_receipts \
                     SET status = 'completed', result_ok = 1, completed_at = 2, updated_at = 2 \
                     WHERE operation_id = ? AND status = 'accepted'",
                )
                .bind(&operation_id)
                .execute(&mut *tx)
                .await
                .unwrap();
                sqlx::query(
                    "UPDATE conversations \
                     SET status = 'finished', active_turn_operation_id = NULL, \
                         admission_epoch = admission_epoch + 1 \
                     WHERE conversation_id = ? AND active_turn_operation_id = ?",
                )
                .bind(conversation_id)
                .bind(operation_id)
                .execute(&mut *tx)
                .await
                .unwrap();
            }
            other => panic!("unsupported exact Conversation test lifecycle status: {other}"),
        }
        tx.commit().await.unwrap();
    }

    fn action_key(
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        turn_generation: u64,
        action_hex: char,
    ) -> IdmmActionReservationKey {
        IdmmActionReservationKey {
            user_id: user_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            turn_id: turn_id.to_owned(),
            turn_generation,
            action_identity: action_hex.to_string().repeat(64),
        }
    }

    fn reserve_params(key: IdmmActionReservationKey, reserved_at: i64) -> ReserveIdmmActionParams {
        ReserveIdmmActionParams { key, reserved_at }
    }

    fn sample_row(
        installation_owner: &str,
        target_kind: &str,
        target_id: &str,
        at: i64,
    ) -> NewIdmmInterventionRow {
        sample_row_for_user(installation_owner, target_kind, target_id, at)
    }

    fn sample_row_for_user(
        user_id: &str,
        target_kind: &str,
        target_id: &str,
        at: i64,
    ) -> NewIdmmInterventionRow {
        NewIdmmInterventionRow {
            intervention_id: nomifun_common::IdmmInterventionId::new().into_string(),
            user_id: user_id.to_string(),
            target_kind: target_kind.to_string(),
            target_id: target_id.to_string(),
            watch: "decision".to_string(),
            at,
            signal: "decision".to_string(),
            tier_used: "rule".to_string(),
            category: Some("option".to_string()),
            action: "answer_choice".to_string(),
            detail: Some("选了方案A".to_string()),
            reason: Some("规则匹配".to_string()),
            confidence: None,
            bypass_model: None,
            outcome: "applied".to_string(),
        }
    }

    #[tokio::test]
    async fn insert_then_list_returns_recent_first() {
        let (repo, _db, owner) = setup().await;
        let first = repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 10))
            .await
            .unwrap();
        let second = repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 30))
            .await
            .unwrap();
        let third = repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 20))
            .await
            .unwrap();
        assert_eq!([first.id, second.id, third.id], [1, 2, 3]);

        let rows = repo
            .list_for_target(&owner, "conversation", CONVERSATION_A, 100)
            .await
            .unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        // 按 at DESC:30 -> 20 -> 10。
        assert_eq!(ids, vec![2, 3, 1]);
    }

    #[tokio::test]
    async fn insert_prunes_to_per_target_cap() {
        let (repo, _db, owner) = setup().await;
        // 插 35 条,at 递增(at=i 对应正整数本地 id=i+1)。
        for i in 0..35 {
            repo.insert(&sample_row(
                &owner,
                "conversation",
                CONVERSATION_A,
                i,
            ))
                .await
                .unwrap();
        }

        let rows = repo
            .list_for_target(&owner, "conversation", CONVERSATION_A, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), PER_TARGET_CAP as usize);
        assert_eq!(rows.len(), 30);

        // 最旧 5 条(at 0..=4)应已被裁掉。
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        for i in 0..5 {
            let stale = i + 1;
            assert!(!ids.contains(&stale), "oldest id {stale} should have been evicted");
        }
        // 最新一条仍在。
        assert!(ids.contains(&35));
        // 最旧的留存项是 at=5。
        let oldest = rows.last().unwrap();
        assert_eq!(oldest.id, 6);
    }

    #[tokio::test]
    async fn delete_for_target_removes_only_that_target() {
        let (repo, _db, owner) = setup().await;
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 10))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 20))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "terminal", TERMINAL_A, 15))
            .await
            .unwrap();

        let removed = repo
            .delete_for_target(&owner, "conversation", CONVERSATION_A)
            .await
            .unwrap();
        assert_eq!(removed, 2);

        assert!(
            repo.list_for_target(&owner, "conversation", CONVERSATION_A, 100)
                .await
                .unwrap()
                .is_empty()
        );
        let remaining = repo
            .list_for_target(&owner, "terminal", TERMINAL_A, 100)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, 3);
    }

    #[tokio::test]
    async fn sweep_removes_older_than_cutoff() {
        let (repo, _db, owner) = setup().await;
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 100))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 1000))
            .await
            .unwrap();

        // cutoff=500:删 at<500(old),留 new。global_cap 足够大不触发硬上限。
        let removed = repo.sweep_all_owners(500, 2000).await.unwrap();
        assert_eq!(removed, 1);

        let rows = repo
            .list_for_target(&owner, "conversation", CONVERSATION_A, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 2);
    }

    #[tokio::test]
    async fn list_recent_is_owner_scoped_cross_target_recent_first_capped() {
        let (repo, _db, owner) = setup().await;
        // 跨多个 target 写入,at 交错。
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 10))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "terminal", TERMINAL_A, 40))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_B, 20))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "terminal", TERMINAL_A, 30))
            .await
            .unwrap();

        // 跨全部 target 按 at DESC:40 -> 30 -> 20 -> 10。
        let rows = repo.list_recent(&owner, 100).await.unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![2, 4, 3, 1]);

        // limit 封顶,仍取最近的。
        let capped = repo.list_recent(&owner, 2).await.unwrap();
        let ids: Vec<i64> = capped.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![2, 4]);
    }

    #[tokio::test]
    async fn clear_all_empties_only_the_owners_activity_and_returns_count() {
        let (repo, db, owner) = setup().await;
        insert_user(&db, OWNER_B).await;
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 10))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "terminal", TERMINAL_A, 20))
            .await
            .unwrap();
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_B, 30))
            .await
            .unwrap();
        repo.insert(&sample_row_for_user(
            OWNER_B,
            "conversation",
            OWNER_B_CONVERSATION,
            40,
        ))
        .await
        .unwrap();

        let removed = repo.clear_all(&owner).await.unwrap();
        assert_eq!(removed, 3);

        assert!(repo.list_recent(&owner, 100).await.unwrap().is_empty());
        let other = repo.list_recent(OWNER_B, 100).await.unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].id, 4);
    }

    #[tokio::test]
    async fn target_queries_and_pruning_are_partitioned_by_owner() {
        let (repo, db, owner) = setup().await;
        insert_user(&db, OWNER_B).await;

        for i in 0..35 {
            repo.insert(&sample_row(
                &owner,
                "conversation",
                CONVERSATION_A,
                i,
            ))
                .await
                .unwrap();
            repo.insert(&sample_row_for_user(
                OWNER_B,
                "conversation",
                OWNER_B_CONVERSATION,
                i,
            ))
            .await
            .unwrap();
        }

        let owner_a = repo
            .list_for_target(&owner, "conversation", CONVERSATION_A, 100)
            .await
            .unwrap();
        let owner_b = repo
            .list_for_target(OWNER_B, "conversation", OWNER_B_CONVERSATION, 100)
            .await
            .unwrap();
        assert_eq!(owner_a.len(), PER_TARGET_CAP as usize);
        assert_eq!(owner_b.len(), PER_TARGET_CAP as usize);
        assert!(owner_a.iter().all(|row| row.user_id == owner));
        assert!(owner_b.iter().all(|row| row.user_id == OWNER_B));
    }

    #[tokio::test]
    async fn intervention_owner_cannot_forge_another_users_target() {
        let (repo, db, _owner) = setup().await;
        insert_user(&db, OWNER_B).await;

        let forged = sample_row_for_user(
            OWNER_B,
            "conversation",
            CONVERSATION_A,
            10,
        );
        let err = repo.insert(&forged).await.unwrap_err();
        assert!(
            err.to_string().contains("IDMM conversation target owner mismatch"),
            "unexpected authority error: {err}"
        );
    }

    #[tokio::test]
    async fn sweep_cap_is_enforced_independently_per_owner() {
        let (repo, db, owner) = setup().await;
        insert_user(&db, OWNER_B).await;
        for i in 0..4 {
            repo.insert(&sample_row(
                &owner,
                "conversation",
                CONVERSATION_A,
                i,
            ))
                .await
                .unwrap();
            repo.insert(&sample_row_for_user(
                OWNER_B,
                "conversation",
                OWNER_B_CONVERSATION,
                i,
            ))
            .await
            .unwrap();
        }

        assert_eq!(repo.sweep_all_owners(i64::MIN, 2).await.unwrap(), 4);
        assert_eq!(repo.list_recent(&owner, 100).await.unwrap().len(), 2);
        assert_eq!(repo.list_recent(OWNER_B, 100).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn exact_action_reservation_is_durable_and_duplicate_is_absorbed() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        let params = reserve_params(
            action_key(&owner, CONVERSATION_A, turn_id.as_str(), 7, 'a'),
            100,
        );

        let first = repo.reserve_action(&params).await.unwrap();
        let IdmmActionReserveResult::Reserved(first_row) = first else {
            panic!("first admission must reserve");
        };
        let duplicate = repo.reserve_action(&params).await.unwrap();
        let IdmmActionReserveResult::AlreadyReserved(duplicate_row) = duplicate else {
            panic!("unsettled duplicate must be absorbed");
        };
        assert_eq!(duplicate_row.reservation_id, first_row.reservation_id);
        assert_eq!(duplicate_row.status, "reserved");
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idmm_action_reservations")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn concurrent_exact_action_reservation_has_one_winner() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        let params = reserve_params(
            action_key(&owner, CONVERSATION_A, turn_id.as_str(), 8, 'a'),
            100,
        );

        let (left, right) = tokio::join!(repo.reserve_action(&params), repo.reserve_action(&params));
        let outcomes = [left.unwrap(), right.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, IdmmActionReserveResult::Reserved(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, IdmmActionReserveResult::AlreadyReserved(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes[0].reservation().reservation_id,
            outcomes[1].reservation().reservation_id
        );
    }

    #[tokio::test]
    async fn terminal_replay_returns_durable_result_without_requiring_running_again() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        let key = action_key(&owner, CONVERSATION_A, turn_id.as_str(), 9, 'b');
        repo.reserve_action(&reserve_params(key.clone(), 100))
            .await
            .unwrap();
        set_conversation_status(&db, CONVERSATION_A, "finished").await;

        assert!(matches!(
            repo.reserve_action(&reserve_params(key.clone(), 101)).await.unwrap(),
            IdmmActionReserveResult::AlreadyReserved(_)
        ));
        let settled = repo
            .settle_action(&key, &IdmmActionSettlement::Applied, 102)
            .await
            .unwrap();
        assert!(matches!(settled, IdmmActionSettleResult::Settled(_)));
        let replay = repo
            .reserve_action(&reserve_params(key, 103))
            .await
            .unwrap();
        let IdmmActionReserveResult::Completed(row) = replay else {
            panic!("settled duplicate must be completed");
        };
        assert_eq!(row.status, "applied");
        assert_eq!(row.settlement_source.as_deref(), Some("execution"));
    }

    #[tokio::test]
    async fn new_reservation_requires_running_conversation_and_valid_owner() {
        let (repo, _db, owner) = setup().await;
        let turn_id = nomifun_common::MessageId::new();
        let pending = reserve_params(
            action_key(&owner, CONVERSATION_A, turn_id.as_str(), 1, 'c'),
            100,
        );
        let error = repo.reserve_action(&pending).await.unwrap_err();
        assert!(error.to_string().contains("requires a Running Conversation"));

        let forged = reserve_params(
            action_key(OWNER_B, CONVERSATION_A, turn_id.as_str(), 1, 'c'),
            100,
        );
        let error = repo.reserve_action(&forged).await.unwrap_err();
        assert!(error.to_string().contains("owner mismatch"));
    }

    #[tokio::test]
    async fn settlement_is_monotonic_and_contradictory_late_result_is_absorbed() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        let key = action_key(&owner, CONVERSATION_A, turn_id.as_str(), 11, 'd');
        repo.reserve_action(&reserve_params(key.clone(), 100))
            .await
            .unwrap();

        let applied = repo
            .settle_action(&key, &IdmmActionSettlement::Applied, 110)
            .await
            .unwrap();
        assert!(matches!(applied, IdmmActionSettleResult::Settled(_)));
        let late_failure = repo
            .settle_action(
                &key,
                &IdmmActionSettlement::Failed {
                    reason: "late transport error".to_owned(),
                },
                120,
            )
            .await
            .unwrap();
        let IdmmActionSettleResult::AlreadySettled(row) = late_failure else {
            panic!("late contradictory settlement must be absorbed");
        };
        assert_eq!(row.status, "applied");
        assert!(row.failure_reason.is_none());
        assert_eq!(row.settled_at, Some(110));
    }

    #[tokio::test]
    async fn exact_turn_recovery_marks_only_unsettled_actions_failed_without_redrive() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        let first = action_key(&owner, CONVERSATION_A, turn_id.as_str(), 13, 'e');
        let second = action_key(&owner, CONVERSATION_A, turn_id.as_str(), 13, 'f');
        let other_generation = action_key(&owner, CONVERSATION_A, turn_id.as_str(), 14, 'e');
        for key in [&first, &second, &other_generation] {
            repo.reserve_action(&reserve_params(key.clone(), 100))
                .await
                .unwrap();
        }
        let known_failure = repo
            .settle_action(
                &first,
                &IdmmActionSettlement::Failed {
                    reason: "known delivery failure".to_owned(),
                },
                105,
            )
            .await
            .unwrap();
        assert_eq!(known_failure.reservation().status, "failed");
        assert_eq!(
            known_failure.reservation().settlement_source.as_deref(),
            Some("execution")
        );

        let exact_turn = IdmmActionTurnIdentity {
            user_id: owner.clone(),
            conversation_id: CONVERSATION_A.to_owned(),
            turn_id: turn_id.as_str().to_owned(),
            turn_generation: 13,
        };
        let unresolved = repo
            .list_reserved_actions_for_turn(&exact_turn)
            .await
            .unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].action_identity, second.action_identity);

        let recovered = repo
            .recover_reserved_actions_for_turn(
                &exact_turn,
                "process restarted with delivery outcome unknown",
                120,
            )
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, "failed");
        assert_eq!(recovered[0].settlement_source.as_deref(), Some("recovery"));
        let recovered_replay = repo
            .reserve_action(&reserve_params(second.clone(), 121))
            .await
            .unwrap();
        let IdmmActionReserveResult::Completed(recovered_row) = recovered_replay else {
            panic!("recovered action replay must remain absorbed");
        };
        assert_eq!(recovered_row.status, "failed");
        assert_eq!(
            recovered_row.settlement_source.as_deref(),
            Some("recovery")
        );
        assert!(repo
            .list_reserved_actions_for_turn(&exact_turn)
            .await
            .unwrap()
            .is_empty());

        let other = repo
            .list_reserved_actions_for_turn(&IdmmActionTurnIdentity {
                turn_generation: 14,
                ..exact_turn
            })
            .await
            .unwrap();
        assert_eq!(other.len(), 1);
    }

    #[tokio::test]
    async fn invalid_or_overflowing_action_identity_is_rejected_before_persistence() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        let mut invalid = action_key(&owner, CONVERSATION_A, turn_id.as_str(), 1, 'a');
        invalid.action_identity = "A".repeat(64);
        assert!(repo
            .reserve_action(&reserve_params(invalid, 100))
            .await
            .unwrap_err()
            .to_string()
            .contains("lowercase SHA-256"));

        let overflow =
            action_key(&owner, CONVERSATION_A, turn_id.as_str(), u64::MAX, 'a');
        assert!(repo
            .reserve_action(&reserve_params(overflow, 100))
            .await
            .unwrap_err()
            .to_string()
            .contains("signed integer range"));
    }

    #[tokio::test]
    async fn reservation_storage_failure_propagates_fail_closed() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        sqlx::query("DROP TABLE idmm_action_reservations")
            .execute(db.pool())
            .await
            .unwrap();
        let turn_id = nomifun_common::MessageId::new();
        let error = repo
            .reserve_action(&reserve_params(
                action_key(&owner, CONVERSATION_A, turn_id.as_str(), 1, 'a'),
                100,
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::Query(_)));
    }

    #[tokio::test]
    async fn audit_clear_and_ttl_sweep_never_remove_action_reservations() {
        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        repo.reserve_action(&reserve_params(
            action_key(&owner, CONVERSATION_A, turn_id.as_str(), 1, 'a'),
            100,
        ))
        .await
        .unwrap();
        repo.insert(&sample_row(&owner, "conversation", CONVERSATION_A, 1))
            .await
            .unwrap();
        repo.delete_for_target(&owner, "conversation", CONVERSATION_A)
            .await
            .unwrap();
        repo.sweep_all_owners(i64::MAX, 1).await.unwrap();

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idmm_action_reservations")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn conversation_delete_is_blocked_by_durable_turn_and_action_history() {
        use crate::repository::conversation::IConversationRepository;
        use crate::repository::sqlite_conversation::SqliteConversationRepository;

        let (repo, db, owner) = setup().await;
        set_conversation_status(&db, CONVERSATION_A, "running").await;
        let turn_id = nomifun_common::MessageId::new();
        repo.reserve_action(&reserve_params(
            action_key(&owner, CONVERSATION_A, turn_id.as_str(), 1, 'a'),
            100,
        ))
        .await
        .unwrap();

        let conversations = SqliteConversationRepository::new(db.pool().clone());
        let error = conversations.delete(CONVERSATION_A).await.unwrap_err();
        assert!(
            matches!(error, DbError::Conflict(_)),
            "a Conversation with exact turn history must not be hard-deleted: {error:?}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM idmm_action_reservations")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "failed deletion must retain the absorbing action reservation"
        );
    }
}
