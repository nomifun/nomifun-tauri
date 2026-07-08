use std::sync::Arc;

use nomifun_api_types::{
    ModelProfile, ModelProfileUpsertRequest, ModelTask, ModelTrait, ProfileSource,
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
