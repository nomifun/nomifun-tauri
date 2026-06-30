use crate::error::DbError;
use crate::models::Provider;

/// Model provider data access abstraction.
///
/// Provides CRUD operations on the `providers` table.
/// API keys are stored encrypted; callers handle encryption/decryption.
#[async_trait::async_trait]
pub trait IProviderRepository: Send + Sync {
    /// Returns all providers, ordered by creation time ascending.
    async fn list(&self) -> Result<Vec<Provider>, DbError>;

    /// Finds a provider by ID, or `None` if not found.
    async fn find_by_id(&self, id: &str) -> Result<Option<Provider>, DbError>;

    /// Creates a new provider and returns the inserted row.
    async fn create(&self, params: CreateProviderParams<'_>) -> Result<Provider, DbError>;

    /// Updates an existing provider. Returns `DbError::NotFound` if the ID doesn't exist.
    async fn update(&self, id: &str, params: UpdateProviderParams<'_>) -> Result<Provider, DbError>;

    /// Deletes a provider by ID. Returns `DbError::NotFound` if the ID doesn't exist.
    async fn delete(&self, id: &str) -> Result<(), DbError>;
}

/// Parameters for creating a new provider.
#[derive(Debug)]
pub struct CreateProviderParams<'a> {
    /// Optional caller-supplied id. When `None`, the repository generates one.
    pub id: Option<&'a str>,
    pub platform: &'a str,
    pub name: &'a str,
    pub base_url: &'a str,
    pub api_key_encrypted: &'a str,
    pub models: &'a str,
    pub enabled: bool,
    pub capabilities: &'a str,
    pub context_limit: Option<i64>,
    pub model_protocols: Option<&'a str>,
    pub model_descriptions: Option<&'a str>,
    pub model_enabled: Option<&'a str>,
    pub model_health: Option<&'a str>,
    pub bedrock_config: Option<&'a str>,
    pub is_full_url: bool,
}

/// Parameters for updating an existing provider.
///
/// All fields are optional; `None` means "keep the current value".
#[derive(Debug, Default)]
pub struct UpdateProviderParams<'a> {
    pub platform: Option<&'a str>,
    pub name: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub api_key_encrypted: Option<&'a str>,
    pub models: Option<&'a str>,
    pub enabled: Option<bool>,
    pub capabilities: Option<&'a str>,
    pub context_limit: Option<Option<i64>>,
    pub model_protocols: Option<Option<&'a str>>,
    pub model_descriptions: Option<Option<&'a str>>,
    pub model_enabled: Option<Option<&'a str>>,
    pub model_health: Option<Option<&'a str>>,
    pub bedrock_config: Option<Option<&'a str>>,
    pub is_full_url: Option<bool>,
}
