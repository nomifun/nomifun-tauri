use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::Provider;
use crate::repository::{
    provider_preference_delete_action, IProviderRepository, ProviderPreferenceDeleteAction,
};
use crate::repository::provider::{CreateProviderParams, UpdateProviderParams};

const PROVIDER_HARD_BINDING_DELETE_CONFLICT: &str =
    "provider is still referenced by an executable Agent binding";

async fn prune_missing_provider_preference(
    key: &str,
    value: String,
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<String, DbError> {
    let mut parsed: serde_json::Value = serde_json::from_str(&value)
        .map_err(|error| DbError::Conflict(format!("invalid client preference '{key}': {error}")))?;
    let items = match key {
        "agent.model_failover" => parsed
            .as_object_mut()
            .and_then(|object| object.get_mut("queue"))
            .and_then(serde_json::Value::as_array_mut),
        "nomi.collaborationModels" => parsed.as_array_mut(),
        _ => None,
    };
    let Some(items) = items else {
        return Ok(value);
    };

    let mut retained = Vec::with_capacity(items.len());
    for item in std::mem::take(items) {
        let Some(provider_id) = item
            .get("provider_id")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM providers WHERE provider_id = ?)",
        )
        .bind(provider_id)
        .fetch_one(&mut **transaction)
        .await?;
        if exists {
            retained.push(item);
        }
    }
    *items = retained;
    Ok(parsed.to_string())
}

/// SQLite-backed implementation of [`IProviderRepository`].
#[derive(Clone, Debug)]
pub struct SqliteProviderRepository {
    pool: SqlitePool,
}

impl SqliteProviderRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IProviderRepository for SqliteProviderRepository {
    async fn list(&self) -> Result<Vec<Provider>, DbError> {
        let rows = sqlx::query_as::<_, Provider>(
            "SELECT * FROM providers ORDER BY sort_order ASC, created_at ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<Provider>, DbError> {
        let row = sqlx::query_as::<_, Provider>("SELECT * FROM providers WHERE provider_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn create(&self, params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
        let provider_id = match params.provider_id {
            Some(provider_id) => nomifun_common::ProviderId::parse(provider_id)
                .map(nomifun_common::ProviderId::into_string)
                .map_err(|error| {
                    DbError::Conflict(format!(
                        "invalid provider_id '{provider_id}': {error}"
                    ))
                })?,
            None => nomifun_common::ProviderId::new().into_string(),
        };
        let now = nomifun_common::now_ms();
        let sort_order = match params.sort_order {
            Some(value) => value,
            None => {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM providers",
                )
                .fetch_one(&self.pool)
                .await?
            }
        };

        sqlx::query(
            "INSERT INTO providers \
                (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                 capabilities, model_context_limits, model_protocols, model_descriptions, \
                 model_enabled, model_health, bedrock_config, is_full_url, sort_order, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&provider_id)
        .bind(params.platform)
        .bind(params.name)
        .bind(params.base_url)
        .bind(params.api_key_encrypted)
        .bind(params.models)
        .bind(params.enabled)
        .bind(params.capabilities)
        .bind(params.model_context_limits.unwrap_or("{}"))
        .bind(params.model_protocols)
        // model_descriptions is NOT NULL DEFAULT '{}'; coalesce None → '{}'.
        .bind(params.model_descriptions.unwrap_or("{}"))
        .bind(params.model_enabled)
        .bind(params.model_health)
        .bind(params.bedrock_config)
        .bind(params.is_full_url)
        .bind(sort_order)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Provider with id '{provider_id}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        Ok(Provider {
            id: sqlx::query_scalar("SELECT id FROM providers WHERE provider_id = ?")
                .bind(&provider_id)
                .fetch_one(&self.pool)
                .await?,
            provider_id,
            platform: params.platform.to_string(),
            name: params.name.to_string(),
            base_url: params.base_url.to_string(),
            api_key_encrypted: params.api_key_encrypted.to_string(),
            models: params.models.to_string(),
            enabled: params.enabled,
            capabilities: params.capabilities.to_string(),
            model_context_limits: params.model_context_limits.map(String::from),
            model_protocols: params.model_protocols.map(String::from),
            model_descriptions: params.model_descriptions.map(String::from),
            model_enabled: params.model_enabled.map(String::from),
            model_health: params.model_health.map(String::from),
            bedrock_config: params.bedrock_config.map(String::from),
            is_full_url: params.is_full_url,
            sort_order,
            created_at: now,
            updated_at: now,
        })
    }

    async fn update(&self, id: &str, params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
        let existing = self
            .find_by_id(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Provider '{id}' not found")))?;

        let merged = merge_update(existing, params);

        sqlx::query(
            "UPDATE providers SET \
                platform = ?, name = ?, base_url = ?, api_key_encrypted = ?, \
                models = ?, enabled = ?, capabilities = ?, \
                model_context_limits = ?, model_protocols = ?, model_descriptions = ?, model_enabled = ?, \
                model_health = ?, bedrock_config = ?, is_full_url = ?, sort_order = ?, updated_at = ? \
             WHERE provider_id = ?",
        )
        .bind(&merged.platform)
        .bind(&merged.name)
        .bind(&merged.base_url)
        .bind(&merged.api_key_encrypted)
        .bind(&merged.models)
        .bind(merged.enabled)
        .bind(&merged.capabilities)
        .bind(merged.model_context_limits.as_deref().unwrap_or("{}"))
        .bind(&merged.model_protocols)
        // model_descriptions is NOT NULL DEFAULT '{}'; coalesce None → '{}'.
        .bind(merged.model_descriptions.as_deref().unwrap_or("{}"))
        .bind(&merged.model_enabled)
        .bind(&merged.model_health)
        .bind(&merged.bedrock_config)
        .bind(merged.is_full_url)
        .bind(merged.sort_order)
        .bind(merged.updated_at)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(merged)
    }

    async fn delete(&self, id: &str) -> Result<(), DbError> {
        let mut transaction = self.pool.begin().await?;

        // Acquire SQLite's writer lock before inspecting logical references.
        // This keeps the guard, provider deletion, and soft-reference cleanup
        // in one application-owned transaction without a physical FK/trigger.
        let locked = sqlx::query(
            "UPDATE providers SET updated_at = updated_at WHERE provider_id = ?",
        )
            .bind(id)
            .execute(&mut *transaction)
            .await?;

        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Provider '{id}' not found")));
        }

        let hard_binding_exists: bool = sqlx::query_scalar(
            "SELECT \
                EXISTS(\
                    SELECT 1 FROM conversations \
                    WHERE json_extract(model, '$.provider_id') = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM agent_execution_template_participants \
                    WHERE provider_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 \
                    FROM agent_execution_participants participant \
                    JOIN agent_executions execution \
                      ON execution.execution_id = participant.execution_id \
                    WHERE participant.provider_id = ?1 \
                      AND participant.retired_in_revision IS NULL \
                      AND execution.status <> 'cancelled' \
                      AND execution.deleted_at IS NULL\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM creation_tasks WHERE provider_id = ?1\
                ) \
                OR EXISTS(\
                    SELECT 1 FROM cron_jobs \
                    WHERE agent_type = 'nomi' \
                      AND agent_config IS NOT NULL \
                      AND CASE \
                            WHEN NOT json_valid(agent_config) THEN 1 \
                            ELSE json_extract(agent_config, '$.provider_id') = ?1 \
                          END\
                )",
        )
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
        if hard_binding_exists {
            return Err(DbError::Conflict(
                PROVIDER_HARD_BINDING_DELETE_CONFLICT.to_owned(),
            ));
        }

        // Client preferences are a generic key/value store, so their Provider
        // references are enforced by the centralized registry in the
        // client-preference repository rather than by SQL FK/trigger logic.
        // Resolve every registered preference before deleting the parent:
        // IDMM backup is RESTRICT; arrays are filtered in order; defaults are
        // deleted and optional references are set to null.
        let preference_rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT key, value FROM client_preferences \
             WHERE key = 'idmm_backup_provider_id' \
                OR key = 'agent.model_failover' \
                OR key = 'nomi.collaborationModels' \
                OR key = 'nomi.defaultModel' \
                OR key = 'knowledge.autogenModel' \
                OR key = 'tools.imageGenerationModel' \
                OR key = 'tools.speechToText' \
                OR key LIKE 'channels.%.defaultModel'",
        )
        .fetch_all(&mut *transaction)
        .await?;
        let mut preference_actions = Vec::new();
        for (key, value) in preference_rows {
            match provider_preference_delete_action(&key, &value, id)? {
                ProviderPreferenceDeleteAction::Keep => {}
                ProviderPreferenceDeleteAction::Restrict => {
                    return Err(DbError::Conflict(
                        "provider is still referenced by an IDMM backup preference".to_owned(),
                    ));
                }
                action => preference_actions.push((key, action)),
            }
        }

        sqlx::query("DELETE FROM providers WHERE provider_id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await?;

        // Soft logical references are repaired explicitly in the same
        // transaction. SQLite owns no cascade or relation semantics.
        sqlx::query(
            "UPDATE conversations \
             SET execution_model_pool = CASE \
                    WHEN json_extract(execution_model_pool, '$.mode') = 'single' \
                        THEN NULL \
                    ELSE (\
                        SELECT CASE \
                            WHEN COUNT(*) = 0 THEN NULL \
                            ELSE json_object(\
                                'mode', 'range', \
                                'models', json(json_group_array(json(item.value)))\
                            ) \
                        END \
                        FROM json_each(conversations.execution_model_pool, '$.models') item \
                        WHERE json_extract(item.value, '$.provider_id') <> ?1 \
                          AND EXISTS (\
                              SELECT 1 FROM providers provider \
                              WHERE provider.provider_id = json_extract(item.value, '$.provider_id')\
                          )\
                    ) \
                 END, \
                 updated_at = MAX(updated_at, ?2) \
             WHERE execution_model_pool IS NOT NULL \
               AND (\
                    (json_extract(execution_model_pool, '$.mode') = 'single' \
                     AND json_extract(execution_model_pool, '$.model.provider_id') = ?1) \
                    OR \
                    (json_extract(execution_model_pool, '$.mode') = 'range' \
                     AND EXISTS (\
                         SELECT 1 FROM json_each(execution_model_pool, '$.models') target \
                         WHERE json_extract(target.value, '$.provider_id') = ?1\
                     ))\
               )",
        )
        .bind(id)
        .bind(nomifun_common::now_ms())
        .execute(&mut *transaction)
        .await?;

        let now = nomifun_common::now_ms();
        sqlx::query(
            "UPDATE conversations \
             SET extra = json_remove(\
                    extra, \
                    CASE \
                        WHEN json_extract(extra, '$.idmm.fault_watch.bypass_model.provider_id') = ?1 \
                        THEN '$.idmm.fault_watch.bypass_model' \
                        ELSE '$.__nomifun_noop_idmm_fault_bypass' \
                    END, \
                    CASE \
                        WHEN json_extract(extra, '$.idmm.decision_watch.bypass_model.provider_id') = ?1 \
                        THEN '$.idmm.decision_watch.bypass_model' \
                        ELSE '$.__nomifun_noop_idmm_decision_bypass' \
                    END\
                 ), \
                 updated_at = MAX(updated_at, ?2) \
             WHERE json_valid(extra) \
               AND (\
                    json_extract(extra, '$.idmm.fault_watch.bypass_model.provider_id') = ?1 \
                    OR json_extract(extra, '$.idmm.decision_watch.bypass_model.provider_id') = ?1\
               )",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE terminal_sessions \
             SET idmm = json_remove(\
                    idmm, \
                    CASE \
                        WHEN json_extract(idmm, '$.fault_watch.bypass_model.provider_id') = ?1 \
                        THEN '$.fault_watch.bypass_model' \
                        ELSE '$.__nomifun_noop_idmm_fault_bypass' \
                    END, \
                    CASE \
                        WHEN json_extract(idmm, '$.decision_watch.bypass_model.provider_id') = ?1 \
                        THEN '$.decision_watch.bypass_model' \
                        ELSE '$.__nomifun_noop_idmm_decision_bypass' \
                    END\
                 ), \
                 updated_at = MAX(updated_at, ?2) \
             WHERE idmm IS NOT NULL \
               AND json_valid(idmm) \
               AND (\
                    json_extract(idmm, '$.fault_watch.bypass_model.provider_id') = ?1 \
                    OR json_extract(idmm, '$.decision_watch.bypass_model.provider_id') = ?1\
               )",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;

        for (key, action) in preference_actions {
            match action {
                ProviderPreferenceDeleteAction::Delete => {
                    sqlx::query("DELETE FROM client_preferences WHERE key = ?")
                        .bind(&key)
                        .execute(&mut *transaction)
                        .await?;
                }
                ProviderPreferenceDeleteAction::Update(value) => {
                    let value =
                        prune_missing_provider_preference(&key, value, &mut transaction).await?;
                    sqlx::query(
                        "UPDATE client_preferences \
                         SET value = ?, updated_at = MAX(updated_at, ?) \
                         WHERE key = ?",
                    )
                    .bind(value)
                    .bind(now)
                    .bind(&key)
                    .execute(&mut *transaction)
                    .await?;
                }
                ProviderPreferenceDeleteAction::Keep
                | ProviderPreferenceDeleteAction::Restrict => unreachable!(),
            }
        }

        sqlx::query("DELETE FROM model_profiles WHERE provider_id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "UPDATE preset_model_preferences SET provider_id = NULL WHERE provider_id = ?",
        )
        .bind(id)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        Ok(())
    }
}

/// Detect SQLite UNIQUE constraint violation (codes 2067 / 1555).
fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "2067" || c == "1555")
}

/// Merge partial update params into an existing provider, returning a new instance.
fn merge_update(existing: Provider, params: UpdateProviderParams<'_>) -> Provider {
    let now = nomifun_common::now_ms();
    Provider {
        id: existing.id,
        provider_id: existing.provider_id,
        platform: params.platform.unwrap_or(&existing.platform).to_string(),
        name: params.name.unwrap_or(&existing.name).to_string(),
        base_url: params.base_url.unwrap_or(&existing.base_url).to_string(),
        api_key_encrypted: params
            .api_key_encrypted
            .unwrap_or(&existing.api_key_encrypted)
            .to_string(),
        models: params.models.unwrap_or(&existing.models).to_string(),
        enabled: params.enabled.unwrap_or(existing.enabled),
        capabilities: params.capabilities.unwrap_or(&existing.capabilities).to_string(),
        model_context_limits: params
            .model_context_limits
            .map_or(existing.model_context_limits, |v| v.map(String::from)),
        model_protocols: params
            .model_protocols
            .map_or(existing.model_protocols, |v| v.map(String::from)),
        model_descriptions: params
            .model_descriptions
            .map_or(existing.model_descriptions, |v| v.map(String::from)),
        model_enabled: params
            .model_enabled
            .map_or(existing.model_enabled, |v| v.map(String::from)),
        model_health: params
            .model_health
            .map_or(existing.model_health, |v| v.map(String::from)),
        bedrock_config: params
            .bedrock_config
            .map_or(existing.bedrock_config, |v| v.map(String::from)),
        is_full_url: params.is_full_url.unwrap_or(existing.is_full_url),
        sort_order: params.sort_order.unwrap_or(existing.sort_order),
        created_at: existing.created_at,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const CALLER_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000010";
    const DUPLICATE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000011";

    async fn setup() -> (SqliteProviderRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteProviderRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_params() -> CreateProviderParams<'static> {
        CreateProviderParams {
            provider_id: None,
            platform: "anthropic",
            name: "Anthropic",
            base_url: "https://api.anthropic.com",
            api_key_encrypted: "encrypted_key_data",
            models: r#"["claude-sonnet-4-20250514"]"#,
            enabled: true,
            capabilities: r#"[{"type":"text"}]"#,
            model_context_limits: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            sort_order: None,
        }
    }

    #[tokio::test]
    async fn list_empty() {
        let (repo, _db) = setup().await;
        let providers = repo.list().await.unwrap();
        assert!(providers.is_empty());
    }

    #[tokio::test]
    async fn create_returns_populated_fields() {
        let (repo, _db) = setup().await;
        let p = repo.create(sample_params()).await.unwrap();

        assert!(nomifun_common::ProviderId::parse(p.provider_id.clone()).is_ok());
        assert_eq!(p.platform, "anthropic");
        assert_eq!(p.name, "Anthropic");
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.api_key_encrypted, "encrypted_key_data");
        assert!(p.enabled);
        assert!(p.model_context_limits.is_none());
        assert!(p.model_protocols.is_none());
        assert!(p.bedrock_config.is_none());
        assert!(p.created_at > 0);
        assert_eq!(p.created_at, p.updated_at);
    }

    #[tokio::test]
    async fn create_with_caller_supplied_id() {
        let (repo, _db) = setup().await;
        let p = repo
            .create(CreateProviderParams {
                provider_id: Some(CALLER_PROVIDER_ID),
                ..sample_params()
            })
            .await
            .unwrap();

        assert_eq!(p.provider_id, CALLER_PROVIDER_ID);
        assert_eq!(p.platform, "anthropic");

        let found = repo.find_by_id(CALLER_PROVIDER_ID).await.unwrap().unwrap();
        assert_eq!(found.provider_id, CALLER_PROVIDER_ID);
    }

    #[tokio::test]
    async fn create_rejects_invalid_caller_supplied_id() {
        let (repo, _db) = setup().await;
        let err = repo
            .create(CreateProviderParams {
                provider_id: Some("my-custom-id-1"),
                ..sample_params()
            })
            .await
            .unwrap_err();

        assert!(
            matches!(err, DbError::Conflict(ref message) if message.contains("invalid provider_id")),
            "expected invalid provider_id conflict, got: {err:?}"
        );
        assert!(repo.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_with_duplicate_caller_id_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create(CreateProviderParams {
            provider_id: Some(DUPLICATE_PROVIDER_ID),
            ..sample_params()
        })
        .await
        .unwrap();

        let err = repo
            .create(CreateProviderParams {
                provider_id: Some(DUPLICATE_PROVIDER_ID),
                ..sample_params()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn create_then_find_by_id() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        let found = repo.find_by_id(&created.provider_id).await.unwrap().unwrap();
        assert_eq!(found.provider_id, created.provider_id);
        assert_eq!(found.platform, "anthropic");
        assert_eq!(found.models, r#"["claude-sonnet-4-20250514"]"#);
    }

    #[tokio::test]
    async fn find_by_id_nonexistent() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_id("no_such_id").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_all_ordered_by_created_at() {
        let (repo, _db) = setup().await;
        let p1 = repo.create(sample_params()).await.unwrap();
        let p2 = repo
            .create(CreateProviderParams {
                platform: "openai",
                name: "OpenAI",
                base_url: "https://api.openai.com",
                ..sample_params()
            })
            .await
            .unwrap();

        let all = repo.list().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].provider_id, p1.provider_id);
        assert_eq!(all[1].provider_id, p2.provider_id);
    }

    #[tokio::test]
    async fn update_partial_fields() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        let updated = repo
            .update(
                &created.provider_id,
                UpdateProviderParams {
                    name: Some("Anthropic Updated"),
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "Anthropic Updated");
        assert!(!updated.enabled);
        // Unchanged fields preserved
        assert_eq!(updated.platform, "anthropic");
        assert_eq!(updated.base_url, "https://api.anthropic.com");
        assert!(updated.updated_at >= created.updated_at);
    }

    #[tokio::test]
    async fn update_api_key() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        let updated = repo
            .update(
                &created.provider_id,
                UpdateProviderParams {
                    api_key_encrypted: Some("new_encrypted_key"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.api_key_encrypted, "new_encrypted_key");
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.update("no_id", UpdateProviderParams::default()).await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_optional_json_fields() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();
        assert!(created.model_protocols.is_none());

        // Set optional field
        let updated = repo
            .update(
                &created.provider_id,
                UpdateProviderParams {
                    model_protocols: Some(Some(r#"{"model1":"openai"}"#)),
                    bedrock_config: Some(Some(r#"{"region":"us-east-1"}"#)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.model_protocols.as_deref(), Some(r#"{"model1":"openai"}"#));
        assert_eq!(updated.bedrock_config.as_deref(), Some(r#"{"region":"us-east-1"}"#));

        // Clear optional field
        let cleared = repo
            .update(
                &created.provider_id,
                UpdateProviderParams {
                    model_protocols: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(cleared.model_protocols.is_none());
        // bedrock_config should still be set
        assert!(cleared.bedrock_config.is_some());
    }

    #[tokio::test]
    async fn delete_existing() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        repo.delete(&created.provider_id).await.unwrap();
        assert!(repo.find_by_id(&created.provider_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete("no_id").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_then_list_excludes_deleted() {
        let (repo, _db) = setup().await;
        let p1 = repo.create(sample_params()).await.unwrap();
        let p2 = repo
            .create(CreateProviderParams {
                name: "Other",
                ..sample_params()
            })
            .await
            .unwrap();

        repo.delete(&p1.provider_id).await.unwrap();

        let all = repo.list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].provider_id, p2.provider_id);
    }

}
