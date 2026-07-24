use nomifun_common::now_ms;
use nomifun_common::ProviderId;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{ModelProfileRow, UpsertModelProfileParams};
use crate::repository::model_profile::IModelProfileRepository;

/// SQLite-backed implementation of [`IModelProfileRepository`].
#[derive(Clone, Debug)]
pub struct SqliteModelProfileRepository {
    pool: SqlitePool,
}

impl SqliteModelProfileRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IModelProfileRepository for SqliteModelProfileRepository {
    async fn list(&self) -> Result<Vec<ModelProfileRow>, DbError> {
        let rows = sqlx::query_as::<_, ModelProfileRow>("SELECT * FROM model_profiles")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn list_for_provider(&self, provider_id: &str) -> Result<Vec<ModelProfileRow>, DbError> {
        let rows = sqlx::query_as::<_, ModelProfileRow>(
            "SELECT * FROM model_profiles WHERE provider_id = ?",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get(&self, provider_id: &str, model: &str) -> Result<Option<ModelProfileRow>, DbError> {
        let row = sqlx::query_as::<_, ModelProfileRow>(
            "SELECT * FROM model_profiles WHERE provider_id = ? AND model = ?",
        )
        .bind(provider_id)
        .bind(model)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert(&self, params: &UpsertModelProfileParams<'_>) -> Result<ModelProfileRow, DbError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let provider_id = ProviderId::parse(params.provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "Model profile provider_id '{}' is not a canonical UUIDv7: {error}",
                params.provider_id
            ))
        })?;
        let parent = sqlx::query("UPDATE providers SET updated_at = updated_at WHERE provider_id = ?")
            .bind(provider_id.as_str())
            .execute(&mut *tx)
            .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Model profile provider '{}' does not exist",
                provider_id
            )));
        }

        sqlx::query(
            "INSERT INTO model_profiles (provider_id, model, tasks, traits, params, source, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(provider_id, model) DO UPDATE SET \
                tasks = excluded.tasks, \
                traits = excluded.traits, \
                params = excluded.params, \
                source = excluded.source, \
                updated_at = excluded.updated_at",
        )
        .bind(provider_id.as_str())
        .bind(params.model)
        .bind(params.tasks)
        .bind(params.traits)
        .bind(params.params)
        .bind(params.source)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM model_profiles WHERE provider_id = ? AND model = ?",
        )
        .bind(provider_id.as_str())
        .bind(params.model)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ModelProfileRow {
            id,
            provider_id: provider_id.into_string(),
            model: params.model.to_string(),
            tasks: params.tasks.to_string(),
            traits: params.traits.to_string(),
            params: params.params.to_string(),
            source: params.source.to_string(),
            updated_at: now,
        })
    }

    async fn insert_if_absent(&self, params: &UpsertModelProfileParams<'_>) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        let provider_id = ProviderId::parse(params.provider_id).map_err(|error| {
            DbError::Conflict(format!(
                "Model profile provider_id '{}' is not a canonical UUIDv7: {error}",
                params.provider_id
            ))
        })?;
        let parent = sqlx::query("UPDATE providers SET updated_at = updated_at WHERE provider_id = ?")
            .bind(provider_id.as_str())
            .execute(&mut *tx)
            .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "Model profile provider '{}' does not exist",
                provider_id
            )));
        }

        let result = sqlx::query(
            "INSERT INTO model_profiles (provider_id, model, tasks, traits, params, source, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(provider_id, model) DO NOTHING",
        )
        .bind(provider_id.as_str())
        .bind(params.model)
        .bind(params.tasks)
        .bind(params.traits)
        .bind(params.params)
        .bind(params.source)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        let inserted = result.rows_affected() > 0;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn delete(&self, provider_id: &str, model: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM model_profiles WHERE provider_id = ? AND model = ?")
            .bind(provider_id)
            .bind(model)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;
    use crate::repository::provider::{CreateProviderParams, IProviderRepository};
    use crate::repository::sqlite_provider::SqliteProviderRepository;

    const PROVIDER_1: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const PROVIDER_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";

    async fn seed_provider(pool: &SqlitePool, provider_id: &str) {
        SqliteProviderRepository::new(pool.clone())
            .create(CreateProviderParams {
                provider_id: Some(provider_id),
                platform: "openai",
                name: provider_id,
                base_url: "https://x.test/v1",
                api_key_encrypted: "enc",
                models: "[]",
                enabled: true,
                capabilities: "[]",
                model_context_limits: None,
                model_protocols: None,
                model_descriptions: None,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url: false,
                sort_order: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn model_profile_upsert_get_list_delete() {
        let db = init_database_memory().await.unwrap();
        seed_provider(db.pool(), PROVIDER_1).await;
        seed_provider(db.pool(), PROVIDER_2).await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());

        r.upsert(&UpsertModelProfileParams {
            provider_id: PROVIDER_1,
            model: "step-image-edit-2",
            tasks: r#"["image_generation","image_edit"]"#,
            traits: "[]",
            params: r#"{"steps":8}"#,
            source: "user",
        })
        .await
        .unwrap();

        let got = r
            .get(PROVIDER_1, "step-image-edit-2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.tasks, r#"["image_generation","image_edit"]"#);
        assert_eq!(got.source, "user");
        assert_eq!(got.params, r#"{"steps":8}"#);

        // Upsert overwrites (same composite key).
        r.upsert(&UpsertModelProfileParams {
            provider_id: PROVIDER_1,
            model: "step-image-edit-2",
            tasks: r#"["image_generation"]"#,
            traits: "[]",
            params: "{}",
            source: "inferred",
        })
        .await
        .unwrap();
        let got = r
            .get(PROVIDER_1, "step-image-edit-2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.tasks, r#"["image_generation"]"#);
        assert_eq!(got.source, "inferred");

        // Second provider row, scoped listing.
        r.upsert(&UpsertModelProfileParams {
            provider_id: PROVIDER_2,
            model: "gpt-4o",
            tasks: r#"["chat"]"#,
            traits: r#"["vision_input"]"#,
            params: "{}",
            source: "inferred",
        })
        .await
        .unwrap();
        assert_eq!(r.list().await.unwrap().len(), 2);
        assert_eq!(r.list_for_provider(PROVIDER_1).await.unwrap().len(), 1);

        assert!(r.delete(PROVIDER_1, "step-image-edit-2").await.unwrap());
        assert!(!r.delete(PROVIDER_1, "step-image-edit-2").await.unwrap());
        assert_eq!(r.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_provider_explicitly_cleans_profiles() {
        let db = init_database_memory().await.unwrap();
        seed_provider(db.pool(), PROVIDER_1).await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());
        r.upsert(&UpsertModelProfileParams {
            provider_id: PROVIDER_1,
            model: "gpt-4o",
            tasks: r#"["chat"]"#,
            traits: "[]",
            params: "{}",
            source: "inferred",
        })
        .await
        .unwrap();
        assert_eq!(r.list().await.unwrap().len(), 1);

        SqliteProviderRepository::new(db.pool().clone())
            .delete(PROVIDER_1)
            .await
            .unwrap();
        assert!(
            r.list().await.unwrap().is_empty(),
            "provider repository must explicitly delete soft child profiles"
        );
    }

    #[tokio::test]
    async fn insert_if_absent_never_overwrites_existing_profile() {
        let db = init_database_memory().await.unwrap();
        seed_provider(db.pool(), PROVIDER_1).await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());

        assert!(
            r.insert_if_absent(&UpsertModelProfileParams {
                provider_id: PROVIDER_1,
                model: "deepseek-v4-flash-free",
                tasks: r#"["chat"]"#,
                traits: r#"["vision_input"]"#,
                params: r#"{"owner":"user"}"#,
                source: "user",
            })
            .await
            .unwrap()
        );
        assert!(
            !r.insert_if_absent(&UpsertModelProfileParams {
                provider_id: PROVIDER_1,
                model: "deepseek-v4-flash-free",
                tasks: r#"["chat"]"#,
                traits: "[]",
                params: "{}",
                source: "inferred",
            })
            .await
            .unwrap()
        );

        let stored = r
            .get(PROVIDER_1, "deepseek-v4-flash-free")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.source, "user");
        assert_eq!(stored.traits, r#"["vision_input"]"#);
        assert_eq!(stored.params, r#"{"owner":"user"}"#);
    }

    #[tokio::test]
    async fn profile_writes_reject_missing_provider_atomically() {
        let db = init_database_memory().await.unwrap();
        let r = SqliteModelProfileRepository::new(db.pool().clone());
        let missing_provider = "0190f5fe-7c00-7a00-8abc-012345678999";
        let params = UpsertModelProfileParams {
            provider_id: missing_provider,
            model: "gpt-4o",
            tasks: r#"["chat"]"#,
            traits: "[]",
            params: "{}",
            source: "inferred",
        };

        assert!(matches!(r.upsert(&params).await, Err(DbError::Conflict(_))));
        assert!(matches!(
            r.insert_if_absent(&params).await,
            Err(DbError::Conflict(_))
        ));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_profiles")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

}
