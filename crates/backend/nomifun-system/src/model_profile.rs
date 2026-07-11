use std::collections::HashSet;
use std::sync::Arc;

use nomifun_api_types::{
    LocalModelCatalogEntry, ModelProfile, ModelProfileUpsertRequest, ModelTask, ModelTrait,
    ProfileSource,
};
use nomifun_common::AppError;
use nomifun_db::{IModelProfileRepository, ModelProfileRow, UpsertModelProfileParams};

/// Business logic for authoritative per-model capability profiles (the
/// multimodal model hub). CRUD only — "resolve models by capability" is
/// composed at the route layer from the provider list + these profiles.
#[derive(Clone)]
pub struct ModelProfileService {
    repo: Arc<dyn IModelProfileRepository>,
}

impl ModelProfileService {
    pub fn new(repo: Arc<dyn IModelProfileRepository>) -> Self {
        Self { repo }
    }

    /// All stored profiles across all providers.
    pub async fn list(&self) -> Result<Vec<ModelProfile>, AppError> {
        let rows = self.repo.list().await?;
        Ok(rows.into_iter().map(row_to_profile).collect())
    }

    /// Insert or replace one profile. `source` defaults to `User` (this is the
    /// user-edit endpoint), making the stored profile authoritative over the
    /// name heuristic.
    pub async fn upsert(&self, req: ModelProfileUpsertRequest) -> Result<ModelProfile, AppError> {
        if req.provider_id.trim().is_empty() {
            return Err(AppError::BadRequest("provider_id is required".into()));
        }
        if req.model.trim().is_empty() {
            return Err(AppError::BadRequest("model is required".into()));
        }
        let tasks_json = serde_json::to_string(&req.tasks)
            .map_err(|e| AppError::Internal(format!("serialize tasks: {e}")))?;
        let traits_json = serde_json::to_string(&req.traits)
            .map_err(|e| AppError::Internal(format!("serialize traits: {e}")))?;
        let params_value = req.params.unwrap_or_else(|| serde_json::json!({}));
        let params_json = serde_json::to_string(&params_value)
            .map_err(|e| AppError::Internal(format!("serialize params: {e}")))?;
        let source = req.source.unwrap_or(ProfileSource::User);
        let source_str = source_to_str(source);

        let row = self
            .repo
            .upsert(&UpsertModelProfileParams {
                provider_id: req.provider_id.trim(),
                model: req.model.trim(),
                tasks: &tasks_json,
                traits: &traits_json,
                params: &params_json,
                source: source_str,
            })
            .await?;
        Ok(row_to_profile(row))
    }

    /// Delete one profile; returns whether a row was removed.
    pub async fn delete(&self, provider_id: &str, model: &str) -> Result<bool, AppError> {
        Ok(self.repo.delete(provider_id, model).await?)
    }

    /// Atomically seed inferred profiles for newly discovered catalog models.
    /// Existing user/catalog/inferred rows are never overwritten.
    pub async fn seed_missing_inferred<S>(
        &self,
        provider_id: &str,
        platform: &str,
        models: &[S],
    ) -> Result<usize, AppError>
    where
        S: AsRef<str> + Sync,
    {
        seed_missing_inferred_profiles(self.repo.as_ref(), provider_id, platform, models).await
    }
}

/// Seed inferred profiles for catalog models that do not already have an
/// authoritative row.
///
/// The repository's atomic `insert_if_absent` primitive is intentional. A
/// refresh runs in the background and may race with a user editing the same
/// profile; a prior `list`/`get` followed by an unconditional upsert could
/// overwrite that newer user choice.
pub async fn seed_missing_inferred_profiles<S>(
    repo: &dyn IModelProfileRepository,
    provider_id: &str,
    platform: &str,
    models: &[S],
) -> Result<usize, AppError>
where
    S: AsRef<str> + Sync,
{
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return Err(AppError::BadRequest("provider_id is required".into()));
    }

    let platform = platform.trim();
    let mut seen = HashSet::new();
    let mut inserted = 0usize;
    for raw_model in models {
        let model = raw_model.as_ref().trim();
        if model.is_empty() || !seen.insert(model.to_owned()) {
            continue;
        }
        let (tasks, traits) = nomifun_api_types::derive_tasks_and_traits(platform, model);
        let tasks_json = serde_json::to_string(&tasks)
            .map_err(|error| AppError::Internal(format!("serialize inferred tasks: {error}")))?;
        let traits_json = serde_json::to_string(&traits)
            .map_err(|error| AppError::Internal(format!("serialize inferred traits: {error}")))?;
        if repo
            .insert_if_absent(&UpsertModelProfileParams {
                provider_id,
                model,
                tasks: &tasks_json,
                traits: &traits_json,
                params: "{}",
                source: "inferred",
            })
            .await?
        {
            inserted += 1;
        }
    }
    Ok(inserted)
}

/// Reconcile NomiFun's curated local catalog into authoritative profiles.
///
/// The repository implements the source-precedence check in one SQL statement:
/// catalog data may replace inferred/older catalog rows, but never a concurrent
/// or existing `source = user` edit.
pub async fn reconcile_local_catalog_profiles(
    repo: &dyn IModelProfileRepository,
    provider_id: &str,
    catalog: &[LocalModelCatalogEntry],
) -> Result<usize, AppError> {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return Err(AppError::BadRequest("provider_id is required".into()));
    }
    let mut changed = 0;
    for entry in catalog {
        let tasks = serde_json::to_string(&entry.tasks)
            .map_err(|error| AppError::Internal(format!("serialize catalog tasks: {error}")))?;
        let traits = serde_json::to_string(&entry.traits)
            .map_err(|error| AppError::Internal(format!("serialize catalog traits: {error}")))?;
        let params = serde_json::to_string(&serde_json::json!({
            "contextWindow": entry.context_window,
            "parameterSize": entry.parameter_size,
            "quantization": entry.quantization,
        }))
        .map_err(|error| AppError::Internal(format!("serialize catalog params: {error}")))?;
        if repo
            .upsert_unless_user(&UpsertModelProfileParams {
                provider_id,
                model: &entry.id,
                tasks: &tasks,
                traits: &traits,
                params: &params,
                source: "catalog",
            })
            .await?
        {
            changed += 1;
        }
    }
    Ok(changed)
}

fn source_to_str(source: ProfileSource) -> &'static str {
    match source {
        ProfileSource::Inferred => "inferred",
        ProfileSource::User => "user",
        ProfileSource::Catalog => "catalog",
    }
}

fn source_from_str(s: &str) -> ProfileSource {
    match s {
        "user" => ProfileSource::User,
        "catalog" => ProfileSource::Catalog,
        _ => ProfileSource::Inferred,
    }
}

/// Map a DB row (JSON-string columns) to the api-types [`ModelProfile`].
/// Malformed JSON degrades gracefully to empty tasks/traits/params rather than
/// erroring, so one bad row never breaks the whole listing.
pub fn row_to_profile(row: ModelProfileRow) -> ModelProfile {
    let tasks: Vec<ModelTask> = serde_json::from_str(&row.tasks).unwrap_or_default();
    let traits: Vec<ModelTrait> = serde_json::from_str(&row.traits).unwrap_or_default();
    let params: serde_json::Value =
        serde_json::from_str(&row.params).unwrap_or_else(|_| serde_json::json!({}));
    ModelProfile {
        provider_id: row.provider_id,
        model: row.model,
        tasks,
        traits,
        params,
        source: source_from_str(&row.source),
        updated_at: row.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{
        CreateProviderParams, IProviderRepository, SqliteModelProfileRepository,
        SqliteProviderRepository, init_database_memory,
    };

    #[tokio::test]
    async fn inferred_seed_is_idempotent_and_preserves_user_profile() {
        let db = init_database_memory().await.unwrap();
        let provider_repo = SqliteProviderRepository::new(db.pool().clone());
        provider_repo
            .create(CreateProviderParams {
                id: Some("managed"),
                platform: "nomifun-free-model",
                name: "Managed",
                base_url: "http://127.0.0.1:1/v1",
                api_key_encrypted: "encrypted",
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
        let profile_repo = SqliteModelProfileRepository::new(db.pool().clone());
        profile_repo
            .upsert(&UpsertModelProfileParams {
                provider_id: "managed",
                model: "big-pickle",
                tasks: r#"["chat"]"#,
                traits: r#"["vision_input"]"#,
                params: r#"{"owner":"user"}"#,
                source: "user",
            })
            .await
            .unwrap();

        let models = vec![
            "big-pickle".to_string(),
            "deepseek-v4-flash-free".to_string(),
            "deepseek-v4-flash-free".to_string(),
            " ".to_string(),
        ];
        assert_eq!(
            seed_missing_inferred_profiles(
                &profile_repo,
                "managed",
                "nomifun-free-model",
                &models,
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            seed_missing_inferred_profiles(
                &profile_repo,
                "managed",
                "nomifun-free-model",
                &models,
            )
            .await
            .unwrap(),
            0
        );

        let user = profile_repo
            .get("managed", "big-pickle")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.source, "user");
        assert_eq!(user.params, r#"{"owner":"user"}"#);
        let inferred = profile_repo
            .get("managed", "deepseek-v4-flash-free")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inferred.source, "inferred");
    }

    #[tokio::test]
    async fn local_catalog_profile_does_not_overwrite_user_authority() {
        let db = init_database_memory().await.unwrap();
        let provider_repo = SqliteProviderRepository::new(db.pool().clone());
        provider_repo
            .create(CreateProviderParams {
                id: Some("nomifun-local-model"),
                platform: "nomifun-local-model",
                name: "Local",
                base_url: "http://127.0.0.1:1/v1",
                api_key_encrypted: "encrypted",
                models: "[]",
                enabled: false,
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
        let profile_repo = SqliteModelProfileRepository::new(db.pool().clone());
        profile_repo
            .upsert(&UpsertModelProfileParams {
                provider_id: "nomifun-local-model",
                model: "local-test",
                tasks: r#"["chat"]"#,
                traits: r#"["function_calling"]"#,
                params: r#"{"owner":"user"}"#,
                source: "user",
            })
            .await
            .unwrap();
        let catalog = vec![LocalModelCatalogEntry {
            id: "local-test".into(),
            name: "Local".into(),
            description: "Local".into(),
            parameter_size: "1B".into(),
            quantization: "Q4_K_M".into(),
            download_size_bytes: 1,
            required_memory_bytes: 2,
            context_window: 4096,
            license: "Apache-2.0".into(),
            source: "test".into(),
            recommended: true,
            tasks: vec![ModelTask::Chat],
            traits: vec![],
        }];
        assert_eq!(
            reconcile_local_catalog_profiles(
                &profile_repo,
                "nomifun-local-model",
                &catalog,
            )
            .await
            .unwrap(),
            0
        );
        let row = profile_repo
            .get("nomifun-local-model", "local-test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source, "user");
        assert_eq!(row.traits, r#"["function_calling"]"#);
    }
}
