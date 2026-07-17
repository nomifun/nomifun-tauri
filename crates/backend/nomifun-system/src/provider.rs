use std::collections::HashMap;
use std::sync::Arc;

use nomifun_api_types::{CreateProviderRequest, ProviderResponse, UpdateProviderRequest};
use nomifun_common::{
    AppError, ProviderId, ProviderInUseDetails, decrypt_string, encrypt_string,
};
use nomifun_db::{CreateProviderParams, IProviderRepository, UpdateProviderParams, models::Provider};
use serde::de::DeserializeOwned;

use crate::managed_model::is_managed_provider_platform;
use crate::provider_deletion::SharedProviderDeletionCoordinator;

/// Business logic for model provider CRUD with API key encryption/masking.
#[derive(Clone)]
pub struct ProviderService {
    repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    coordinator: Option<SharedProviderDeletionCoordinator>,
}

impl ProviderService {
    pub fn new(repo: Arc<dyn IProviderRepository>, encryption_key: [u8; 32]) -> Self {
        Self {
            repo,
            encryption_key,
            coordinator: None,
        }
    }

    /// Inject a deletion coordinator so `delete` returns friendly labeled
    /// conflicts before SQLite enforces the same hard bindings atomically.
    pub fn with_deletion_coordinator(mut self, coordinator: SharedProviderDeletionCoordinator) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// List all providers with masked API keys.
    pub async fn list(&self) -> Result<Vec<ProviderResponse>, AppError> {
        let rows = self.repo.list().await?;
        rows.into_iter().map(|row| self.row_to_response(row)).collect()
    }

    /// Create a new provider. The API key is encrypted before storage.
    ///
    /// If `req.id` is `Some`, the caller-supplied canonical `prov_<uuid-v7>` is
    /// used after strict validation; otherwise the repository generates one.
    pub async fn create(&self, req: CreateProviderRequest) -> Result<ProviderResponse, AppError> {
        reject_managed_create(&req)?;
        validate_create_request(&req)?;

        let encrypted_key = encrypt_string(&req.api_key, &self.encryption_key)?;
        let models_json = serialize_json(&req.models, "models")?;
        let capabilities_json = serialize_json(&req.capabilities, "capabilities")?;
        let model_protocols_json = serialize_opt(&req.model_protocols, "model_protocols")?;
        let model_context_limits_json = serialize_opt(&req.model_context_limits, "model_context_limits")?;
        let model_descriptions_json = serialize_opt(&req.model_descriptions, "model_descriptions")?;
        let model_enabled_json = serialize_opt(&req.model_enabled, "model_enabled")?;
        let model_health_json = serialize_opt(&req.model_health, "model_health")?;
        let bedrock_json = serialize_opt(&req.bedrock_config, "bedrock_config")?;
        let params = CreateProviderParams {
            id: req.id.as_deref(),
            platform: &req.platform,
            name: &req.name,
            base_url: &req.base_url,
            api_key_encrypted: &encrypted_key,
            models: &models_json,
            enabled: req.enabled,
            capabilities: &capabilities_json,
            context_limit: req.context_limit,
            model_context_limits: model_context_limits_json.as_deref(),
            model_protocols: model_protocols_json.as_deref(),
            model_descriptions: model_descriptions_json.as_deref(),
            model_enabled: model_enabled_json.as_deref(),
            model_health: model_health_json.as_deref(),
            bedrock_config: bedrock_json.as_deref(),
            is_full_url: req.is_full_url,
            sort_order: req.sort_order,
        };

        let row = self.repo.create(params).await?;
        self.row_to_response(row)
    }

    /// Update an existing provider. Only provided fields are changed.
    pub async fn update(&self, id: &str, req: UpdateProviderRequest) -> Result<ProviderResponse, AppError> {
        validate_id(id)?;
        self.reject_persisted_managed_provider(id).await?;
        reject_managed_update(&req)?;
        validate_update_request(&req)?;

        let encrypted_key = req
            .api_key
            .as_deref()
            .map(|k| encrypt_string(k, &self.encryption_key))
            .transpose()?;
        let models_json = serialize_opt(&req.models, "models")?;
        let capabilities_json = serialize_opt(&req.capabilities, "capabilities")?;
        let model_protocols_json = serialize_opt(&req.model_protocols, "model_protocols")?;
        let model_context_limits_json = serialize_opt(&req.model_context_limits, "model_context_limits")?;
        let model_descriptions_json = serialize_opt(&req.model_descriptions, "model_descriptions")?;
        let model_enabled_json = serialize_opt(&req.model_enabled, "model_enabled")?;
        let model_health_json = serialize_opt(&req.model_health, "model_health")?;
        let bedrock_json = serialize_opt(&req.bedrock_config, "bedrock_config")?;

        let params = UpdateProviderParams {
            platform: req.platform.as_deref(),
            name: req.name.as_deref(),
            base_url: req.base_url.as_deref(),
            api_key_encrypted: encrypted_key.as_deref(),
            models: models_json.as_deref(),
            enabled: req.enabled,
            capabilities: capabilities_json.as_deref(),
            context_limit: req.context_limit.map(Some),
            model_context_limits: model_context_limits_json.as_ref().map(|s| Some(s.as_str())),
            model_protocols: model_protocols_json.as_ref().map(|s| Some(s.as_str())),
            model_descriptions: model_descriptions_json.as_ref().map(|s| Some(s.as_str())),
            model_enabled: model_enabled_json.as_ref().map(|s| Some(s.as_str())),
            model_health: model_health_json.as_ref().map(|s| Some(s.as_str())),
            bedrock_config: bedrock_json.as_ref().map(|s| Some(s.as_str())),
            is_full_url: req.is_full_url,
            sort_order: req.sort_order,
        };

        let row = self.repo.update(id, params).await?;
        self.row_to_response(row)
    }

    /// Delete a provider by ID.
    ///
    /// When a deletion coordinator is configured, deletion is refused with
    /// `AppError::ProviderInUse` if any feature still holds a hard binding to
    /// the provider. SQLite owns atomic soft-reference cleanup in the provider
    /// DELETE transaction; there is no post-commit cleanup phase.
    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        validate_id(id)?;
        self.reject_persisted_managed_provider(id).await?;
        if let Some(coord) = &self.coordinator {
            let usages = coord.usages(id).await?;
            if !usages.is_empty() {
                return Err(AppError::ProviderInUse(ProviderInUseDetails { usages }));
            }
        }
        self.repo.delete(id).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn reject_persisted_managed_provider(&self, id: &str) -> Result<(), AppError> {
        if let Some(row) = self.repo.find_by_id(id).await?
            && is_managed_provider_platform(&row.platform)
        {
            return Err(AppError::Forbidden(
                "Managed model providers must be changed through their dedicated model-service API"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Convert a DB row into a response DTO with the plaintext API key
    /// (decrypted) and deserialized JSON fields.
    ///
    /// Pre-launch: the response returns the API key in plaintext so the
    /// frontend can migrate its local store to the backend without losing
    /// the key on re-read. Storage remains encrypted at rest.
    fn row_to_response(&self, row: Provider) -> Result<ProviderResponse, AppError> {
        ProviderId::parse(&row.id).map_err(|error| {
            AppError::Internal(format!(
                "stored providers.id '{}' is not canonical: {error}",
                row.id
            ))
        })?;
        let managed = is_managed_provider_platform(&row.platform);
        let api_key = if managed {
            String::new()
        } else {
            decrypt_string(&row.api_key_encrypted, &self.encryption_key)?
        };

        let models: Vec<String> = serde_json::from_str(&row.models)
            .map_err(|e| AppError::Internal(format!("Failed to parse models JSON: {e}")))?;
        let capabilities = serde_json::from_str(&row.capabilities)
            .map_err(|e| AppError::Internal(format!("Failed to parse capabilities JSON: {e}")))?;
        let model_protocols: Option<HashMap<String, String>> =
            deserialize_opt(&row.model_protocols, "model_protocols")?;
        let model_context_limits: Option<HashMap<String, i64>> =
            deserialize_opt(&row.model_context_limits, "model_context_limits")?;
        let model_context_limits =
            merge_legacy_model_context_limits(&models, row.context_limit, model_context_limits);
        let model_descriptions: Option<HashMap<String, String>> =
            deserialize_opt(&row.model_descriptions, "model_descriptions")?;
        let model_enabled: Option<HashMap<String, bool>> = deserialize_opt(&row.model_enabled, "model_enabled")?;
        let model_health = deserialize_opt(&row.model_health, "model_health")?;
        let bedrock_config = deserialize_opt(&row.bedrock_config, "bedrock_config")?;

        Ok(ProviderResponse {
            id: row.id,
            platform: row.platform,
            name: row.name,
            base_url: row.base_url,
            api_key,
            models,
            enabled: row.enabled,
            capabilities,
            context_limit: row.context_limit,
            model_context_limits,
            model_protocols,
            model_descriptions,
            model_enabled,
            model_health,
            bedrock_config,
            is_full_url: row.is_full_url,
            sort_order: row.sort_order,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

// ---------------------------------------------------------------------------
// JSON helpers (M-1 / M-2 refactor)
// ---------------------------------------------------------------------------

/// Serialize an optional value to JSON string.
fn serialize_opt<T: serde::Serialize>(val: &Option<T>, field: &str) -> Result<Option<String>, AppError> {
    val.as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| AppError::Internal(format!("Failed to serialize {field}: {e}")))
}

/// Serialize a value to JSON string.
fn serialize_json<T: serde::Serialize>(val: &T, field: &str) -> Result<String, AppError> {
    serde_json::to_string(val).map_err(|e| AppError::Internal(format!("Failed to serialize {field}: {e}")))
}

fn merge_legacy_model_context_limits(
    models: &[String],
    legacy_context_limit: Option<i64>,
    model_context_limits: Option<HashMap<String, i64>>,
) -> Option<HashMap<String, i64>> {
    let mut limits = model_context_limits.unwrap_or_default();
    if limits.is_empty()
        && let Some(limit) = legacy_context_limit.filter(|value| *value > 0)
    {
        for model in models {
            limits.insert(model.clone(), limit);
        }
    }
    if limits.is_empty() { None } else { Some(limits) }
}

/// Deserialize an optional JSON string into a typed value.
pub(crate) fn deserialize_opt<T: DeserializeOwned>(json: &Option<String>, field: &str) -> Result<Option<T>, AppError> {
    json.as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| AppError::Internal(format!("Failed to parse {field} JSON: {e}")))
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_create_request(req: &CreateProviderRequest) -> Result<(), AppError> {
    if let Some(ref id) = req.id {
        validate_id(id)?;
    }
    if req.platform.trim().is_empty() {
        return Err(AppError::BadRequest("platform is required".into()));
    }
    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("name is required".into()));
    }
    validate_sort_order(req.sort_order)?;
    // Bedrock auths via bedrock_config (IAM profile / static keys) rather than
    // an HTTP endpoint + bearer key, so baseUrl and apiKey may be empty.
    if req.platform == "bedrock" {
        if req.bedrock_config.is_none() {
            return Err(AppError::BadRequest(
                "bedrockConfig is required for bedrock platform".into(),
            ));
        }
        if !req.base_url.trim().is_empty() {
            validate_base_url(&req.base_url)?;
        }
    } else {
        validate_base_url(&req.base_url)?;
        if req.api_key.trim().is_empty() {
            return Err(AppError::BadRequest("apiKey is required".into()));
        }
    }
    Ok(())
}

fn reject_managed_create(req: &CreateProviderRequest) -> Result<(), AppError> {
    if is_managed_provider_platform(req.platform.trim()) {
        return Err(AppError::Forbidden(
            "Managed model providers are created and configured by their dedicated model-service API"
                .into(),
        ));
    }
    Ok(())
}

fn reject_managed_update(req: &UpdateProviderRequest) -> Result<(), AppError> {
    if req
        .platform
        .as_deref()
        .map(str::trim)
        .is_some_and(is_managed_provider_platform)
    {
        return Err(AppError::Forbidden(
            "Managed model providers must be changed through their dedicated model-service API"
                .into(),
        ));
    }
    Ok(())
}

/// Validate a caller-supplied provider id.
///
fn validate_id(id: &str) -> Result<(), AppError> {
    ProviderId::parse(id)
        .map(|_| ())
        .map_err(|error| AppError::BadRequest(format!("invalid provider id: {error}")))
}

fn validate_update_request(req: &UpdateProviderRequest) -> Result<(), AppError> {
    validate_sort_order(req.sort_order)?;
    if let Some(ref platform) = req.platform
        && platform.trim().is_empty()
    {
        return Err(AppError::BadRequest("platform cannot be empty".into()));
    }
    if let Some(ref name) = req.name
        && name.trim().is_empty()
    {
        return Err(AppError::BadRequest("name cannot be empty".into()));
    }
    if let Some(ref url) = req.base_url
        && !url.trim().is_empty()
    {
        validate_base_url(url)?;
    }
    Ok(())
}

fn validate_sort_order(sort_order: Option<i64>) -> Result<(), AppError> {
    if sort_order.is_some_and(|value| value < 0) {
        return Err(AppError::BadRequest("sort_order must be non-negative".into()));
    }
    Ok(())
}

fn validate_base_url(url: &str) -> Result<(), AppError> {
    if url.trim().is_empty() {
        return Err(AppError::BadRequest("baseUrl is required".into()));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(AppError::BadRequest(
            "baseUrl must start with http:// or https://".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{SqliteProviderRepository, init_database_memory};

    // A fixed 32-byte key for testing
    const TEST_KEY: [u8; 32] = [0x42; 32];

    async fn setup() -> ProviderService {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        std::mem::forget(db);
        ProviderService::new(repo, TEST_KEY)
    }

    fn sample_create_request() -> CreateProviderRequest {
        CreateProviderRequest {
            id: None,
            platform: "anthropic".into(),
            name: "Anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "sk-ant-api03-test1234".into(),
            models: vec!["claude-sonnet-4-20250514".into()],
            enabled: true,
            capabilities: vec![],
            context_limit: None,
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

    // -- id validation tests --

    #[test]
    fn validate_id_accepts_canonical_provider_uuid_v7() {
        assert!(
            validate_id("prov_0190f5fe-7c00-7a00-8abc-012345678901").is_ok()
        );
    }

    #[test]
    fn validate_id_rejects_uuid_v4() {
        assert!(validate_id("prov_11111111-1111-4111-8111-111111111111").is_err());
    }

    #[test]
    fn validate_id_rejects_legacy_short_hex() {
        assert!(validate_id("a1b2c3d4").is_err());
    }

    #[test]
    fn validate_id_rejects_empty() {
        assert!(validate_id("").is_err());
        assert!(validate_id("   ").is_err());
    }

    #[test]
    fn validate_id_rejects_wrong_prefix_or_noncanonical_characters() {
        assert!(
            validate_id("provider_0190f5fe-7c00-7a00-8abc-012345678901").is_err()
        );
        assert!(
            validate_id("prov_019ABCDEF012-7ABC-8ABC-0123-456789ABCDEF").is_err()
        );
        assert!(validate_id("bad/slash").is_err());
    }

    // -- validation tests --

    #[test]
    fn validate_create_missing_platform() {
        let req = CreateProviderRequest {
            platform: "".into(),
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_err());
    }

    #[test]
    fn validate_create_missing_name() {
        let req = CreateProviderRequest {
            name: "  ".into(),
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_err());
    }

    #[test]
    fn validate_create_missing_base_url() {
        let req = CreateProviderRequest {
            base_url: "".into(),
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_err());
    }

    #[test]
    fn validate_create_invalid_url() {
        let req = CreateProviderRequest {
            base_url: "not-a-url".into(),
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_err());
    }

    #[test]
    fn validate_create_missing_api_key() {
        let req = CreateProviderRequest {
            api_key: "  ".into(),
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_err());
    }

    #[test]
    fn validate_create_valid() {
        assert!(validate_create_request(&sample_create_request()).is_ok());
    }

    #[test]
    fn generic_create_rejects_managed_platform() {
        let by_id = CreateProviderRequest {
            id: Some(nomifun_common::ProviderId::new().into_string()),
            ..sample_create_request()
        };
        assert!(reject_managed_create(&by_id).is_ok());

        let by_platform = CreateProviderRequest {
            platform: crate::managed_model::FREE_MODEL_PLATFORM.into(),
            ..sample_create_request()
        };
        assert!(matches!(
            reject_managed_create(&by_platform),
            Err(AppError::Forbidden(_))
        ));
    }

    #[test]
    fn generic_update_rejects_managed_platform() {
        assert!(matches!(
            reject_managed_update(&UpdateProviderRequest {
                platform: Some(crate::managed_model::FREE_MODEL_PLATFORM.into()),
                ..Default::default()
            }),
            Err(AppError::Forbidden(_))
        ));
    }

    #[test]
    fn validate_create_bedrock_allows_empty_base_url_and_api_key() {
        let req = CreateProviderRequest {
            platform: "bedrock".into(),
            name: "AWS Bedrock".into(),
            base_url: "".into(),
            api_key: "".into(),
            bedrock_config: Some(nomifun_api_types::BedrockConfig {
                auth_method: nomifun_api_types::BedrockAuthMethod::Profile,
                region: "us-west-2".into(),
                profile: Some("ai".into()),
                access_key_id: None,
                secret_access_key: None,
            }),
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_ok());
    }

    #[test]
    fn validate_create_bedrock_requires_bedrock_config() {
        let req = CreateProviderRequest {
            platform: "bedrock".into(),
            name: "AWS Bedrock".into(),
            base_url: "".into(),
            api_key: "".into(),
            bedrock_config: None,
            ..sample_create_request()
        };
        assert!(validate_create_request(&req).is_err());
    }

    #[test]
    fn validate_update_empty_name_rejected() {
        let req = UpdateProviderRequest {
            name: Some("".into()),
            ..Default::default()
        };
        assert!(validate_update_request(&req).is_err());
    }

    #[test]
    fn validate_update_empty_request_ok() {
        assert!(validate_update_request(&UpdateProviderRequest::default()).is_ok());
    }

    #[test]
    fn validate_update_empty_base_url_ok() {
        let req = UpdateProviderRequest {
            base_url: Some("".into()),
            ..Default::default()
        };
        assert!(validate_update_request(&req).is_ok());
    }

    #[test]
    fn validate_update_invalid_base_url_rejected() {
        let req = UpdateProviderRequest {
            base_url: Some("not-a-url".into()),
            ..Default::default()
        };
        assert!(validate_update_request(&req).is_err());
    }

    #[test]
    fn validate_base_url_http() {
        assert!(validate_base_url("http://localhost:8080").is_ok());
    }

    #[test]
    fn validate_base_url_https() {
        assert!(validate_base_url("https://api.example.com").is_ok());
    }

    #[test]
    fn validate_base_url_ftp_rejected() {
        assert!(validate_base_url("ftp://files.example.com").is_err());
    }

    // -- service integration tests --

    #[tokio::test]
    async fn list_empty() {
        let svc = setup().await;
        let result = svc.list().await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn create_and_list() {
        let svc = setup().await;
        let created = svc.create(sample_create_request()).await.unwrap();

        assert!(created.id.starts_with("prov_"));
        assert_eq!(created.platform, "anthropic");
        assert_eq!(created.name, "Anthropic");
        assert_eq!(created.base_url, "https://api.anthropic.com");
        // API key is returned in plaintext (pre-launch; encrypted at rest).
        assert_eq!(created.api_key, "sk-ant-api03-test1234");
        assert_eq!(created.models, vec!["claude-sonnet-4-20250514"]);
        assert!(created.enabled);

        let all = svc.list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, created.id);
        assert_eq!(all[0].api_key, "sk-ant-api03-test1234");
    }

    #[tokio::test]
    async fn create_with_provided_id() {
        let svc = setup().await;
        let provider_id = ProviderId::new().into_string();
        let req = CreateProviderRequest {
            id: Some(provider_id.clone()),
            ..sample_create_request()
        };
        let created = svc.create(req).await.unwrap();
        assert_eq!(created.id, provider_id);
    }

    #[tokio::test]
    async fn create_with_provided_id_rejects_invalid() {
        let svc = setup().await;
        let req = CreateProviderRequest {
            id: Some("   ".into()),
            ..sample_create_request()
        };
        let err = svc.create(req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_with_duplicate_id_returns_conflict() {
        let svc = setup().await;
        let provider_id = ProviderId::new().into_string();
        let req1 = CreateProviderRequest {
            id: Some(provider_id.clone()),
            ..sample_create_request()
        };
        svc.create(req1).await.unwrap();

        let req2 = CreateProviderRequest {
            id: Some(provider_id),
            ..sample_create_request()
        };
        let err = svc.create(req2).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn create_persists_per_model_fields() {
        use std::collections::HashMap;
        let svc = setup().await;
        let req = CreateProviderRequest {
            model_protocols: Some(HashMap::from([("gpt-4".into(), "openai".into())])),
            model_enabled: Some(HashMap::from([("gpt-4".into(), true), ("gpt-3.5".into(), false)])),
            ..sample_create_request()
        };
        let created = svc.create(req).await.unwrap();

        assert_eq!(
            created.model_protocols.as_ref().and_then(|m| m.get("gpt-4")),
            Some(&"openai".to_string())
        );
        assert_eq!(created.model_enabled.as_ref().and_then(|m| m.get("gpt-4")), Some(&true));
        assert_eq!(
            created.model_enabled.as_ref().and_then(|m| m.get("gpt-3.5")),
            Some(&false)
        );

        // And persist through a fresh read.
        let all = svc.list().await.unwrap();
        assert_eq!(all[0].model_enabled.as_ref().and_then(|m| m.get("gpt-4")), Some(&true));
    }

    #[tokio::test]
    async fn create_and_update_round_trips_model_descriptions() {
        use std::collections::HashMap;
        let svc = setup().await;

        // create with a model description map
        let req = CreateProviderRequest {
            model_descriptions: Some(HashMap::from([("m1".into(), "擅长前端".into())])),
            ..sample_create_request()
        };
        let created = svc.create(req).await.unwrap();
        assert_eq!(
            created.model_descriptions.as_ref().and_then(|m| m.get("m1")),
            Some(&"擅长前端".to_string())
        );

        // persists through a fresh read (row_to_response decode path)
        let all = svc.list().await.unwrap();
        assert_eq!(
            all[0].model_descriptions.as_ref().and_then(|m| m.get("m1")),
            Some(&"擅长前端".to_string())
        );

        // update changes the description
        let updated = svc
            .update(
                &created.id,
                UpdateProviderRequest {
                    model_descriptions: Some(HashMap::from([("m1".into(), "擅长后端".into())])),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            updated.model_descriptions.as_ref().and_then(|m| m.get("m1")),
            Some(&"擅长后端".to_string())
        );
    }

    #[tokio::test]
    async fn create_and_update_round_trips_model_context_limits() {
        use std::collections::HashMap;
        let svc = setup().await;

        let req = CreateProviderRequest {
            models: vec!["m1".into(), "m2".into()],
            model_context_limits: Some(HashMap::from([("m1".into(), 32_000), ("m2".into(), 128_000)])),
            ..sample_create_request()
        };
        let created = svc.create(req).await.unwrap();
        assert_eq!(
            created.model_context_limits.as_ref().and_then(|m| m.get("m2")),
            Some(&128_000)
        );

        let all = svc.list().await.unwrap();
        assert_eq!(
            all[0].model_context_limits.as_ref().and_then(|m| m.get("m1")),
            Some(&32_000)
        );

        let updated = svc
            .update(
                &created.id,
                UpdateProviderRequest {
                    model_context_limits: Some(HashMap::from([("m2".into(), 200_000)])),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            updated.model_context_limits.as_ref().and_then(|m| m.get("m2")),
            Some(&200_000)
        );
        assert!(updated.model_context_limits.as_ref().and_then(|m| m.get("m1")).is_none());

        let cleared = svc
            .update(
                &created.id,
                UpdateProviderRequest {
                    model_context_limits: Some(HashMap::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(cleared.model_context_limits.is_none());
    }

    #[tokio::test]
    async fn provider_response_api_key_plaintext_matches_input() {
        // Replaces the masking test: api_key on the response is the
        // encrypted-then-decrypted plaintext (equal to the input).
        let svc = setup().await;
        let req = CreateProviderRequest {
            api_key: "sk-secret-original-value".into(),
            ..sample_create_request()
        };
        let created = svc.create(req).await.unwrap();
        assert_eq!(created.api_key, "sk-secret-original-value");
        assert!(!created.api_key.contains("***"));
    }

    #[tokio::test]
    async fn managed_provider_secret_is_redacted_from_generic_list() {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let encrypted = encrypt_string("internal-loopback-token", &TEST_KEY).unwrap();
        let provider_id = nomifun_common::ProviderId::new().into_string();
        repo.create(CreateProviderParams {
            id: Some(&provider_id),
            platform: crate::managed_model::FREE_MODEL_PLATFORM,
            name: "NomiFun Free Model",
            base_url: "http://127.0.0.1:12345/v1",
            api_key_encrypted: &encrypted,
            models: r#"["big-pickle"]"#,
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
        let svc = ProviderService::new(repo, TEST_KEY);
        let providers = svc.list().await.unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, provider_id);
        assert!(providers[0].api_key.is_empty());
    }

    #[tokio::test]
    async fn persisted_managed_platform_is_protected_by_update_and_delete() {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let encrypted = encrypt_string("internal-loopback-token", &TEST_KEY).unwrap();
        let provider_id = nomifun_common::ProviderId::new().into_string();
        repo.create(CreateProviderParams {
            id: Some(&provider_id),
            platform: crate::managed_model::FREE_MODEL_PLATFORM,
            name: "Managed provider",
            base_url: "http://127.0.0.1:12345/v1",
            api_key_encrypted: &encrypted,
            models: r#"["big-pickle"]"#,
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
        let svc = ProviderService::new(repo, TEST_KEY);

        assert!(matches!(
            svc.update(
                &provider_id,
                UpdateProviderRequest {
                    name: Some("changed".into()),
                    ..Default::default()
                }
            )
            .await,
            Err(AppError::Forbidden(_))
        ));
        assert!(matches!(
            svc.delete(&provider_id).await,
            Err(AppError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn create_invalid_request_rejected() {
        let svc = setup().await;
        let req = CreateProviderRequest {
            platform: "".into(),
            ..sample_create_request()
        };
        let err = svc.create(req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn update_name() {
        let svc = setup().await;
        let created = svc.create(sample_create_request()).await.unwrap();

        let updated = svc
            .update(
                &created.id,
                UpdateProviderRequest {
                    name: Some("New Name".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "New Name");
        assert_eq!(updated.platform, "anthropic");
    }

    #[tokio::test]
    async fn update_api_key_re_encrypts() {
        let svc = setup().await;
        let created = svc.create(sample_create_request()).await.unwrap();

        let updated = svc
            .update(
                &created.id,
                UpdateProviderRequest {
                    api_key: Some("new-key-abcdefgh".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Response carries the new plaintext key (encrypted at rest).
        assert_eq!(updated.api_key, "new-key-abcdefgh");
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let svc = setup().await;
        let err = svc
            .update(
                "prov_0190f5fe-7c00-7a00-8000-000000000099",
                UpdateProviderRequest::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_existing() {
        let svc = setup().await;
        let created = svc.create(sample_create_request()).await.unwrap();

        svc.delete(&created.id).await.unwrap();
        let all = svc.list().await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let svc = setup().await;
        let err = svc
            .delete("prov_0190f5fe-7c00-7a00-8000-000000000099")
            .await
            .unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod delete_guard_tests {
    use super::*;
    use nomifun_common::{ProviderUsage, ProviderUsageFeature};
    use nomifun_db::{SqliteProviderRepository, init_database_memory, models::Provider};
    use std::sync::atomic::{AtomicBool, Ordering};

    const PROVIDER_ID: &str = "prov_0190f5fe-7c00-7a00-8000-000000000098";

    struct CountingRepo {
        deleted: AtomicBool,
    }
    #[async_trait::async_trait]
    impl IProviderRepository for CountingRepo {
        async fn list(&self) -> Result<Vec<Provider>, nomifun_db::DbError> {
            Ok(vec![])
        }
        async fn find_by_id(&self, _: &str) -> Result<Option<Provider>, nomifun_db::DbError> {
            Ok(None)
        }
        async fn create(&self, _: nomifun_db::CreateProviderParams<'_>) -> Result<Provider, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn update(&self, _: &str, _: nomifun_db::UpdateProviderParams<'_>) -> Result<Provider, nomifun_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _: &str) -> Result<(), nomifun_db::DbError> {
            self.deleted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeCoord {
        usages: Vec<ProviderUsage>,
    }
    #[async_trait::async_trait]
    impl crate::provider_deletion::ProviderDeletionCoordinator for FakeCoord {
        async fn usages(&self, _: &str) -> Result<Vec<ProviderUsage>, AppError> {
            Ok(self.usages.clone())
        }
    }

    struct RacingCoord {
        pool: nomifun_db::sqlx::SqlitePool,
    }

    #[async_trait::async_trait]
    impl crate::provider_deletion::ProviderDeletionCoordinator for RacingCoord {
        async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError> {
            // Simulate a hard binding committed immediately after the friendly
            // application scan observed no usages. The provider DELETE trigger
            // remains the authoritative race barrier.
            nomifun_db::sqlx::query(
                "INSERT INTO conversations (\
                    id, user_id, name, type, extra, model, status, pinned, created_at, updated_at\
                 ) VALUES (\
                    'conv_0190f5fe-7c00-7a00-8000-000000000098', \
                    (SELECT owner_user_id FROM installation_identity WHERE key = 'installation'), \
                    'racing conversation', 'nomi', '{}', ?,\
                    'pending', 0, 1, 1\
                 )",
            )
            .bind(format!(
                r#"{{"provider_id":"{provider_id}","model":"model"}}"#
            ))
            .execute(&self.pool)
            .await
            .map_err(|error| AppError::Internal(format!("create racing provider binding: {error}")))?;
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn delete_blocked_when_in_use() {
        let repo = Arc::new(CountingRepo {
            deleted: AtomicBool::new(false),
        });
        let coord = Arc::new(FakeCoord {
            usages: vec![ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: "甲".into(),
                target_id: None,
            }],
        });
        let svc = ProviderService::new(repo.clone(), [0u8; 32]).with_deletion_coordinator(coord);
        let err = svc.delete(PROVIDER_ID).await.unwrap_err();
        assert!(matches!(err, AppError::ProviderInUse(_)));
        assert!(!repo.deleted.load(Ordering::SeqCst), "must not delete when in use");
    }

    #[tokio::test]
    async fn delete_proceeds_when_unused() {
        let repo = Arc::new(CountingRepo {
            deleted: AtomicBool::new(false),
        });
        let coord = Arc::new(FakeCoord {
            usages: vec![],
        });
        let svc = ProviderService::new(repo.clone(), [0u8; 32]).with_deletion_coordinator(coord);
        svc.delete(PROVIDER_ID).await.unwrap();
        assert!(repo.deleted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn delete_race_is_reported_as_conflict_instead_of_internal_error() {
        let database = init_database_memory().await.unwrap();
        nomifun_db::sqlx::query(
            "INSERT INTO providers (\
                id, platform, name, base_url, api_key_encrypted, models, enabled,\
                capabilities, created_at, updated_at\
             ) VALUES (\
                'prov_0190f5fe-7c00-7a00-8000-000000000097', 'openai', 'Race provider', 'https://example.invalid',\
                'encrypted', '[]', 1, '[]', 1, 1\
             )",
        )
        .execute(database.pool())
        .await
        .unwrap();

        let repo = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        let coordinator = Arc::new(RacingCoord {
            pool: database.pool().clone(),
        });
        let service =
            ProviderService::new(repo.clone(), [0u8; 32]).with_deletion_coordinator(coordinator);

        let provider_id = "prov_0190f5fe-7c00-7a00-8000-000000000097";
        let error = service.delete(provider_id).await.unwrap_err();
        assert!(
            matches!(
                error,
                AppError::Conflict(ref message)
                    if message == "provider is still referenced by an executable Agent binding"
            ),
            "the atomic delete guard must surface as a deterministic conflict; got {error:?}"
        );
        assert_eq!(error.status_code(), axum::http::StatusCode::CONFLICT);
        assert!(repo.find_by_id(provider_id).await.unwrap().is_some());
    }
}
