use crate::error::DbError;
use crate::models::{ModelProfileRow, UpsertModelProfileParams};

/// CRUD for authoritative per-model capability profiles, keyed by
/// `(provider_id, model)`.
#[async_trait::async_trait]
pub trait IModelProfileRepository: Send + Sync {
    /// All profiles across all providers.
    async fn list(&self) -> Result<Vec<ModelProfileRow>, DbError>;
    /// Profiles for one provider.
    async fn list_for_provider(&self, provider_id: &str) -> Result<Vec<ModelProfileRow>, DbError>;
    /// A single profile, if present.
    async fn get(&self, provider_id: &str, model: &str) -> Result<Option<ModelProfileRow>, DbError>;
    /// Insert or replace a profile.
    async fn upsert(&self, params: &UpsertModelProfileParams<'_>) -> Result<ModelProfileRow, DbError>;
    /// Delete one profile; returns whether a row was removed.
    async fn delete(&self, provider_id: &str, model: &str) -> Result<bool, DbError>;
}
