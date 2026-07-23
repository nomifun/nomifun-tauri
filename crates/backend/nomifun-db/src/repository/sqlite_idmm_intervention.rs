use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{IdmmInterventionRow, NewIdmmInterventionRow};
use crate::repository::idmm_intervention::{IIdmmInterventionRepository, PER_TARGET_CAP};

#[derive(Clone, Debug)]
pub struct SqliteIdmmInterventionRepository {
    pool: SqlitePool,
}

impl SqliteIdmmInterventionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
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
}
