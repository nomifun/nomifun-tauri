use nomifun_common::now_ms;
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
        .bind(params.provider_id)
        .bind(params.model)
        .bind(params.tasks)
        .bind(params.traits)
        .bind(params.params)
        .bind(params.source)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(ModelProfileRow {
            provider_id: params.provider_id.to_string(),
            model: params.model.to_string(),
            tasks: params.tasks.to_string(),
            traits: params.traits.to_string(),
            params: params.params.to_string(),
            source: params.source.to_string(),
            updated_at: now,
        })
    }

    async fn insert_if_absent(&self, params: &UpsertModelProfileParams<'_>) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO model_profiles (provider_id, model, tasks, traits, params, source, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(provider_id, model) DO NOTHING",
        )
        .bind(params.provider_id)
        .bind(params.model)
        .bind(params.tasks)
        .bind(params.traits)
        .bind(params.params)
        .bind(params.source)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert_unless_user(&self, params: &UpsertModelProfileParams<'_>) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO model_profiles (provider_id, model, tasks, traits, params, source, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(provider_id, model) DO UPDATE SET \
                tasks = excluded.tasks, \
                traits = excluded.traits, \
                params = excluded.params, \
                source = excluded.source, \
                updated_at = excluded.updated_at \
             WHERE model_profiles.source <> 'user'",
        )
        .bind(params.provider_id)
        .bind(params.model)
        .bind(params.tasks)
        .bind(params.traits)
        .bind(params.params)
        .bind(params.source)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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

    async fn seed_provider(pool: &SqlitePool, id: &str) {
        SqliteProviderRepository::new(pool.clone())
            .create(CreateProviderParams {
                id: Some(id),
                platform: "openai",
                name: id,
                base_url: "https://x.test/v1",
                api_key_encrypted: "enc",
                models: "[]",
                enabled: true,
                capabilities: "[]",
                context_limit: None,
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
        seed_provider(db.pool(), "p1").await;
        seed_provider(db.pool(), "p2").await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());

        r.upsert(&UpsertModelProfileParams {
            provider_id: "p1",
            model: "step-image-edit-2",
            tasks: r#"["image_generation","image_edit"]"#,
            traits: "[]",
            params: r#"{"steps":8}"#,
            source: "user",
        })
        .await
        .unwrap();

        let got = r.get("p1", "step-image-edit-2").await.unwrap().unwrap();
        assert_eq!(got.tasks, r#"["image_generation","image_edit"]"#);
        assert_eq!(got.source, "user");
        assert_eq!(got.params, r#"{"steps":8}"#);

        // Upsert overwrites (same composite key).
        r.upsert(&UpsertModelProfileParams {
            provider_id: "p1",
            model: "step-image-edit-2",
            tasks: r#"["image_generation"]"#,
            traits: "[]",
            params: "{}",
            source: "inferred",
        })
        .await
        .unwrap();
        let got = r.get("p1", "step-image-edit-2").await.unwrap().unwrap();
        assert_eq!(got.tasks, r#"["image_generation"]"#);
        assert_eq!(got.source, "inferred");

        // Second provider row, scoped listing.
        r.upsert(&UpsertModelProfileParams {
            provider_id: "p2",
            model: "gpt-4o",
            tasks: r#"["chat"]"#,
            traits: r#"["vision_input"]"#,
            params: "{}",
            source: "inferred",
        })
        .await
        .unwrap();
        assert_eq!(r.list().await.unwrap().len(), 2);
        assert_eq!(r.list_for_provider("p1").await.unwrap().len(), 1);

        assert!(r.delete("p1", "step-image-edit-2").await.unwrap());
        assert!(!r.delete("p1", "step-image-edit-2").await.unwrap());
        assert_eq!(r.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_provider_cascades_profiles() {
        let db = init_database_memory().await.unwrap();
        seed_provider(db.pool(), "p1").await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());
        r.upsert(&UpsertModelProfileParams {
            provider_id: "p1",
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
            .delete("p1")
            .await
            .unwrap();
        assert!(r.list().await.unwrap().is_empty(), "profiles should cascade-delete with provider");
    }

    #[tokio::test]
    async fn insert_if_absent_never_overwrites_existing_profile() {
        let db = init_database_memory().await.unwrap();
        seed_provider(db.pool(), "p1").await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());

        assert!(
            r.insert_if_absent(&UpsertModelProfileParams {
                provider_id: "p1",
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
                provider_id: "p1",
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
            .get("p1", "deepseek-v4-flash-free")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.source, "user");
        assert_eq!(stored.traits, r#"["vision_input"]"#);
        assert_eq!(stored.params, r#"{"owner":"user"}"#);
    }

    #[tokio::test]
    async fn catalog_upsert_replaces_inferred_but_preserves_user() {
        let db = init_database_memory().await.unwrap();
        seed_provider(db.pool(), "p1").await;
        let r = SqliteModelProfileRepository::new(db.pool().clone());

        r.upsert(&UpsertModelProfileParams {
            provider_id: "p1",
            model: "inferred-model",
            tasks: r#"["chat"]"#,
            traits: r#"["reasoning"]"#,
            params: "{}",
            source: "inferred",
        })
        .await
        .unwrap();
        assert!(
            r.upsert_unless_user(&UpsertModelProfileParams {
                provider_id: "p1",
                model: "inferred-model",
                tasks: r#"["chat"]"#,
                traits: "[]",
                params: r#"{"contextWindow":4096}"#,
                source: "catalog",
            })
            .await
            .unwrap()
        );
        assert_eq!(
            r.get("p1", "inferred-model").await.unwrap().unwrap().source,
            "catalog"
        );

        r.upsert(&UpsertModelProfileParams {
            provider_id: "p1",
            model: "user-model",
            tasks: r#"["chat"]"#,
            traits: r#"["function_calling"]"#,
            params: r#"{"owner":"user"}"#,
            source: "user",
        })
        .await
        .unwrap();
        assert!(
            !r.upsert_unless_user(&UpsertModelProfileParams {
                provider_id: "p1",
                model: "user-model",
                tasks: r#"["chat"]"#,
                traits: "[]",
                params: "{}",
                source: "catalog",
            })
            .await
            .unwrap()
        );
        let user = r.get("p1", "user-model").await.unwrap().unwrap();
        assert_eq!(user.source, "user");
        assert_eq!(user.params, r#"{"owner":"user"}"#);
    }
}
