use std::collections::{BTreeMap, BTreeSet};

use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::ClientPreference;
use crate::repository::IClientPreferenceRepository;
use crate::repository::client_preference::normalize_provider_preference;

/// SQLite-backed implementation of [`IClientPreferenceRepository`].
#[derive(Clone, Debug)]
pub struct SqliteClientPreferenceRepository {
    pool: SqlitePool,
}

impl SqliteClientPreferenceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn apply_batch(
        &self,
        upserts: &[(&str, &str)],
        delete_keys: &[&str],
    ) -> Result<(), DbError> {
        if upserts.is_empty() && delete_keys.is_empty() {
            return Ok(());
        }

        // A key can occur more than once in lower-level callers. Only its last
        // value is final and therefore eligible for validation/persistence.
        let final_upserts: BTreeMap<&str, &str> =
            upserts.iter().copied().collect();
        let delete_keys: BTreeSet<&str> = delete_keys
            .iter()
            .copied()
            .filter(|key| !final_upserts.contains_key(key))
            .collect();

        let mut normalized_upserts = Vec::with_capacity(final_upserts.len());
        let mut provider_ids = BTreeSet::new();
        for (key, value) in final_upserts {
            let normalized = normalize_provider_preference(key, value)?;
            provider_ids.extend(normalized.provider_ids);
            normalized_upserts.push((key, normalized.value));
        }

        let mut tx = self.pool.begin().await?;

        // Lock and validate every logical Provider parent before any preference
        // mutation. SQLite's single writer lock then serializes this batch with
        // Provider deletion through commit.
        for provider_id in provider_ids {
            let parent = sqlx::query(
                "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
            )
            .bind(&provider_id)
            .execute(&mut *tx)
            .await?;
            if parent.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "Provider '{provider_id}' referenced by client preferences does not exist"
                )));
            }
        }

        for key in delete_keys {
            sqlx::query("DELETE FROM client_preferences WHERE key = ?")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }

        let now = nomifun_common::now_ms();
        for (key, value) in normalized_upserts {
            sqlx::query(
                "INSERT INTO client_preferences (key, value, updated_at) \
                 VALUES (?, ?, ?) \
                 ON CONFLICT(key) DO UPDATE SET \
                    value = excluded.value, \
                    updated_at = excluded.updated_at",
            )
            .bind(key)
            .bind(value)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl IClientPreferenceRepository for SqliteClientPreferenceRepository {
    async fn get_all(&self) -> Result<Vec<ClientPreference>, DbError> {
        let rows = sqlx::query_as::<_, ClientPreference>("SELECT * FROM client_preferences ORDER BY key")
            .fetch_all(&self.pool)
            .await?;

        Ok(rows)
    }

    async fn get_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
        if keys.is_empty() {
            return Ok(vec![]);
        }

        // Build dynamic IN clause with positional placeholders
        let placeholders: Vec<&str> = keys.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT * FROM client_preferences WHERE key IN ({}) ORDER BY key",
            placeholders.join(", ")
        );

        let mut query = sqlx::query_as::<_, ClientPreference>(&sql);
        for key in keys {
            query = query.bind(*key);
        }

        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows)
    }

    async fn upsert_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError> {
        self.apply_batch(entries, &[]).await
    }

    async fn delete_keys(&self, keys: &[&str]) -> Result<(), DbError> {
        self.apply_batch(&[], keys).await
    }

    async fn update_batch(
        &self,
        upserts: &[(&str, &str)],
        delete_keys: &[&str],
    ) -> Result<(), DbError> {
        self.apply_batch(upserts, delete_keys).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteClientPreferenceRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteClientPreferenceRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn get_all_empty() {
        let (repo, _db) = setup().await;
        let prefs = repo.get_all().await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn upsert_and_get_all() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(&[("theme", "\"dark\""), ("companion.size", "360")])
            .await
            .unwrap();

        let prefs = repo.get_all().await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs[0].key, "companion.size");
        assert_eq!(prefs[0].value, "360");
        assert_eq!(prefs[1].key, "theme");
        assert_eq!(prefs[1].value, "\"dark\"");
    }

    #[tokio::test]
    async fn get_by_keys_filters_correctly() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(&[("a", "1"), ("b", "2"), ("c", "3")]).await.unwrap();

        let prefs = repo.get_by_keys(&["a", "c", "nonexistent"]).await.unwrap();
        assert_eq!(prefs.len(), 2);

        let keys: Vec<&str> = prefs.iter().map(|p| p.key.as_str()).collect();
        assert!(keys.contains(&"a"));
        assert!(keys.contains(&"c"));
    }

    #[tokio::test]
    async fn get_by_keys_empty_input() {
        let (repo, _db) = setup().await;
        let prefs = repo.get_by_keys(&[]).await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn upsert_overwrites_existing_key() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(&[("k", "v1")]).await.unwrap();
        repo.upsert_batch(&[("k", "v2")]).await.unwrap();

        let prefs = repo.get_all().await.unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].value, "v2");
    }

    #[tokio::test]
    async fn idmm_backup_provider_requires_existing_logical_parent() {
        let (repo, db) = setup().await;
        let missing = "0190f5fe-7c00-7a00-8000-000000000003";

        assert!(matches!(
            repo.upsert_batch(&[("idmm_backup_provider_id", missing)])
                .await,
            Err(DbError::Conflict(_))
        ));
        assert!(
            repo.get_by_keys(&["idmm_backup_provider_id"])
                .await
                .unwrap()
                .is_empty()
        );

        sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', 'logical parent', 'https://example.invalid', \
                       'encrypted', '[]', 1, '[]', 1, 1)",
        )
        .bind(missing)
        .execute(db.pool())
        .await
        .unwrap();

        repo.upsert_batch(&[("idmm_backup_provider_id", missing)])
            .await
            .unwrap();
        assert_eq!(
            repo.get_by_keys(&["idmm_backup_provider_id"])
                .await
                .unwrap()[0]
                .value,
            missing
        );
    }

    #[tokio::test]
    async fn provider_reference_validation_rolls_back_the_entire_batch() {
        let (repo, _db) = setup().await;
        let missing = "0190f5fe-7c00-7a00-8000-000000000003";
        let model = serde_json::json!({
            "provider_id": missing,
            "model": "model"
        })
        .to_string();

        assert!(matches!(
            repo.upsert_batch(&[
                ("theme", "\"dark\""),
                ("knowledge.autogenModel", model.as_str()),
            ])
            .await,
            Err(DbError::Conflict(_))
        ));
        assert!(repo.get_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_keys_removes_entries() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(&[("a", "1"), ("b", "2"), ("c", "3")]).await.unwrap();

        repo.delete_keys(&["a", "c"]).await.unwrap();

        let prefs = repo.get_all().await.unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].key, "b");
    }

    #[tokio::test]
    async fn delete_keys_nonexistent_is_noop() {
        let (repo, _db) = setup().await;
        repo.delete_keys(&["ghost"]).await.unwrap();
        assert!(repo.get_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_keys_empty_input() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(&[("x", "1")]).await.unwrap();
        repo.delete_keys(&[]).await.unwrap();
        assert_eq!(repo.get_all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn upsert_empty_batch_is_noop() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(&[]).await.unwrap();
        assert!(repo.get_all().await.unwrap().is_empty());
    }
}
