use std::collections::HashSet;
use std::sync::Arc;

use nomifun_api_types::{
    ModelProfile, ModelProfileUpsertRequest, ModelTask, ModelTrait, ProfileSource,
};
use nomifun_common::{AppError, ProviderId};
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
        rows.into_iter().map(row_to_profile).collect()
    }

    /// Insert or replace one profile. `source` defaults to `User` (this is the
    /// user-edit endpoint), making the stored profile authoritative over the
    /// name heuristic.
    pub async fn upsert(&self, req: ModelProfileUpsertRequest) -> Result<ModelProfile, AppError> {
        let provider_id = ProviderId::parse(req.provider_id)
            .map_err(|error| AppError::BadRequest(format!("invalid provider_id: {error}")))?
            .into_string();
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
                provider_id: &provider_id,
                model: req.model.trim(),
                tasks: &tasks_json,
                traits: &traits_json,
                params: &params_json,
                source: source_str,
            })
            .await?;
        row_to_profile(row)
    }

    /// Delete one profile; returns whether a row was removed.
    pub async fn delete(&self, provider_id: &str, model: &str) -> Result<bool, AppError> {
        ProviderId::parse(provider_id)
            .map_err(|error| AppError::BadRequest(format!("invalid provider_id: {error}")))?;
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
    let provider_id = ProviderId::parse(provider_id)
        .map_err(|error| AppError::BadRequest(format!("invalid provider_id: {error}")))?;

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
                provider_id: provider_id.as_str(),
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

fn source_to_str(source: ProfileSource) -> &'static str {
    match source {
        ProfileSource::Inferred => "inferred",
        ProfileSource::User => "user",
    }
}

fn source_from_str(s: &str) -> ProfileSource {
    match s {
        "user" => ProfileSource::User,
        _ => ProfileSource::Inferred,
    }
}

/// Map a DB row (JSON-string columns) to the api-types [`ModelProfile`].
/// Malformed JSON degrades gracefully to empty tasks/traits/params rather than
/// erroring, so one bad row never breaks the whole listing.
pub fn row_to_profile(row: ModelProfileRow) -> Result<ModelProfile, AppError> {
    ProviderId::parse(&row.provider_id).map_err(|error| {
        AppError::Internal(format!("invalid canonical provider ID in model profile: {error}"))
    })?;
    let tasks: Vec<ModelTask> = serde_json::from_str(&row.tasks)
        .map_err(|error| AppError::Internal(format!("invalid model profile tasks JSON: {error}")))?;
    let traits: Vec<ModelTrait> = serde_json::from_str(&row.traits)
        .map_err(|error| AppError::Internal(format!("invalid model profile traits JSON: {error}")))?;
    let params: serde_json::Value = serde_json::from_str(&row.params)
        .map_err(|error| AppError::Internal(format!("invalid model profile params JSON: {error}")))?;
    Ok(ModelProfile {
        provider_id: row.provider_id,
        model: row.model,
        tasks,
        traits,
        params,
        source: source_from_str(&row.source),
        updated_at: row.updated_at,
    })
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
        let provider_id = ProviderId::new().into_string();
        provider_repo
            .create(CreateProviderParams {
                id: Some(&provider_id),
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
                provider_id: &provider_id,
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
                &provider_id,
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
                &provider_id,
                "nomifun-free-model",
                &models,
            )
            .await
            .unwrap(),
            0
        );

        let user = profile_repo
            .get(&provider_id, "big-pickle")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.source, "user");
        assert_eq!(user.params, r#"{"owner":"user"}"#);
        let inferred = profile_repo
            .get(&provider_id, "deepseek-v4-flash-free")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inferred.source, "inferred");
    }

}
