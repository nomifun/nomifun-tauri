use sqlx::SqlitePool;
use nomifun_common::CompanionId;

use crate::error::DbError;
use crate::repository::ICompanionTokenRepository;

/// SQLite-backed [`ICompanionTokenRepository`]. Keyed on `companion_id`;
/// `upsert_for_companion` rotates a companion's single token row.
#[derive(Clone, Debug)]
pub struct SqliteCompanionTokenRepository {
    pool: SqlitePool,
}

impl SqliteCompanionTokenRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ICompanionTokenRepository for SqliteCompanionTokenRepository {
    async fn list_all(&self) -> Result<Vec<(CompanionId, String)>, DbError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT companion_id, token_hash FROM companion_access_token",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, hash)| {
                CompanionId::parse(id.clone())
                    .map(|id| (id, hash))
                    .map_err(|error| DbError::Init(format!("invalid companion access-token owner '{id}': {error}")))
            })
            .collect()
    }

    async fn upsert_for_companion(&self, companion_id: &CompanionId, token_hash: &str) -> Result<(), DbError> {
        let now = nomifun_common::now_ms();
        sqlx::query(
            "INSERT INTO companion_access_token (companion_id, token_hash, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(companion_id) DO UPDATE SET token_hash = ?2, created_at = ?3",
        )
        .bind(companion_id.as_str())
        .bind(token_hash)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_for_companion(&self, companion_id: &CompanionId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM companion_access_token WHERE companion_id = ?1")
            .bind(companion_id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    #[tokio::test]
    async fn companion_token_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteCompanionTokenRepository::new(db.pool().clone());

        // Empty until minted.
        assert!(repo.list_all().await.unwrap().is_empty());

        let companion_a = CompanionId::new();
        let companion_b = CompanionId::new();
        repo.upsert_for_companion(&companion_a, "hash-a").await.unwrap();
        repo.upsert_for_companion(&companion_b, "hash-b").await.unwrap();
        let mut all = repo.list_all().await.unwrap();
        all.sort();
        assert_eq!(
            all,
            vec![
                (companion_a.clone(), "hash-a".to_string()),
                (companion_b.clone(), "hash-b".to_string()),
            ]
        );

        // Re-mint for the same companion rotates its hash (keyed on companion_id).
        repo.upsert_for_companion(&companion_a, "hash-a2").await.unwrap();
        let all = repo.list_all().await.unwrap();
        assert!(all.contains(&(companion_a.clone(), "hash-a2".to_string())));
        assert_eq!(all.len(), 2);

        // Revocation is idempotent.
        repo.delete_for_companion(&companion_a).await.unwrap();
        repo.delete_for_companion(&companion_a).await.unwrap();
        let all = repo.list_all().await.unwrap();
        assert_eq!(all, vec![(companion_b, "hash-b".to_string())]);
    }

    #[tokio::test]
    async fn list_all_rejects_noncanonical_persisted_owner() {
        let db = init_database_memory().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO companion_access_token (companion_id, token_hash, created_at) VALUES ('1', 'hash', 1)")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(db.pool())
            .await
            .unwrap();
        let repo = SqliteCompanionTokenRepository::new(db.pool().clone());
        assert!(matches!(repo.list_all().await, Err(DbError::Init(_))));
    }
}
