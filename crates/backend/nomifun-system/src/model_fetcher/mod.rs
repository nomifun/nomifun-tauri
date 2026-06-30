mod fetchers;
mod url_fixer;

use std::sync::Arc;

use nomifun_api_types::{BedrockConfig, FetchModelsAnonymousRequest, FetchModelsRequest, FetchModelsResponse};
use nomifun_common::{AppError, decrypt_string};
use nomifun_db::IProviderRepository;

use crate::provider::deserialize_opt;

/// Internal configuration extracted from a provider row for model fetching.
#[derive(Debug)]
pub(crate) struct FetchConfig {
    pub platform: String,
    pub base_url: String,
    pub api_key: String,
    pub bedrock_config: Option<BedrockConfig>,
}

/// Service for fetching model lists from remote provider APIs.
#[derive(Clone)]
pub struct ModelFetchService {
    repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    http_client: reqwest::Client,
}

impl ModelFetchService {
    pub fn new(repo: Arc<dyn IProviderRepository>, encryption_key: [u8; 32], http_client: reqwest::Client) -> Self {
        Self {
            repo,
            encryption_key,
            http_client,
        }
    }

    /// Fetch models for a provider by ID. If `try_fix` is true and the
    /// initial request fails on an OpenAI-compatible platform, attempt
    /// URL auto-correction with parallel probing.
    pub async fn fetch_models(
        &self,
        provider_id: &str,
        req: &FetchModelsRequest,
    ) -> Result<FetchModelsResponse, AppError> {
        let config = self.load_provider_config(provider_id).await?;
        self.fetch_with_config(&config, req.try_fix).await
    }

    /// Fetch models using credentials supplied in the request, without a
    /// persisted provider row. Powers the pre-create "Fetch Models" preview
    /// in the Add-Platform form.
    pub async fn fetch_models_anonymous(
        &self,
        req: &FetchModelsAnonymousRequest,
    ) -> Result<FetchModelsResponse, AppError> {
        validate_anonymous_request(req)?;
        let config = FetchConfig {
            platform: req.platform.clone(),
            base_url: req.base_url.clone(),
            api_key: req.api_key.clone(),
            bedrock_config: req.bedrock_config.clone(),
        };
        self.fetch_with_config(&config, req.try_fix).await
    }

    /// Shared fetch+try_fix branch used by both the by-id and anonymous
    /// entry points.
    async fn fetch_with_config(&self, config: &FetchConfig, try_fix: bool) -> Result<FetchModelsResponse, AppError> {
        match fetchers::fetch_for_platform(&self.http_client, config).await {
            Ok(models) => Ok(FetchModelsResponse {
                models,
                fixed_base_url: None,
            }),
            Err(err) if try_fix && supports_url_fix(&config.platform) => {
                url_fixer::try_fix_url(&self.http_client, config).await.map_err(|_| err)
            }
            Err(err) => Err(err),
        }
    }

    /// Extract and decrypt provider configuration from DB.
    async fn load_provider_config(&self, provider_id: &str) -> Result<FetchConfig, AppError> {
        let row = self
            .repo
            .find_by_id(provider_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Provider {provider_id} not found")))?;

        let api_key = decrypt_string(&row.api_key_encrypted, &self.encryption_key)?;
        if api_key.trim().is_empty() {
            return Err(AppError::BadRequest("API key is empty".into()));
        }

        let bedrock_config: Option<BedrockConfig> = deserialize_opt(&row.bedrock_config, "bedrock_config")?;

        Ok(FetchConfig {
            platform: row.platform,
            base_url: row.base_url,
            api_key,
            bedrock_config,
        })
    }
}

/// Validate a `FetchModelsAnonymousRequest` — platform / base_url / api_key
/// must all be non-empty after trim.
fn validate_anonymous_request(req: &FetchModelsAnonymousRequest) -> Result<(), AppError> {
    if req.platform.trim().is_empty() {
        return Err(AppError::BadRequest("platform is required".into()));
    }
    if req.base_url.trim().is_empty() {
        return Err(AppError::BadRequest("baseUrl is required".into()));
    }
    // Bedrock uses bedrock_config for credentials; empty api_key is allowed there.
    if req.platform != "bedrock" && req.api_key.trim().is_empty() {
        return Err(AppError::BadRequest("apiKey is required".into()));
    }
    Ok(())
}

/// Platforms that support URL auto-fix (OpenAI-compatible).
fn supports_url_fix(platform: &str) -> bool {
    !matches!(
        platform,
        "anthropic" | "claude" | "gemini" | "bedrock" | "vertex-ai" | "minimax" | "dashscope-coding"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::encrypt_string;
    use nomifun_db::{CreateProviderParams, SqliteProviderRepository, init_database_memory};

    const TEST_KEY: [u8; 32] = [0x42; 32];

    async fn setup() -> (ModelFetchService, nomifun_db::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let svc = ModelFetchService::new(repo, TEST_KEY, reqwest::Client::new());
        (svc, db)
    }

    async fn create_provider(db: &nomifun_db::Database, platform: &str, base_url: &str, api_key: &str) -> String {
        let repo = SqliteProviderRepository::new(db.pool().clone());
        let encrypted = encrypt_string(api_key, &TEST_KEY).unwrap();
        let row = repo
            .create(CreateProviderParams {
                id: None,
                platform,
                name: "Test",
                base_url,
                api_key_encrypted: &encrypted,
                models: "[]",
                enabled: true,
                capabilities: "[]",
                context_limit: None,
                model_protocols: None,
                model_descriptions: None,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url: false,
            })
            .await
            .unwrap();
        row.id
    }

    #[test]
    fn supports_url_fix_openai_compatible() {
        assert!(supports_url_fix("openai"));
        assert!(supports_url_fix("new-api"));
        assert!(supports_url_fix("some-custom-provider"));
    }

    #[test]
    fn supports_url_fix_non_openai() {
        assert!(!supports_url_fix("anthropic"));
        assert!(!supports_url_fix("claude"));
        assert!(!supports_url_fix("gemini"));
        assert!(!supports_url_fix("bedrock"));
        assert!(!supports_url_fix("vertex-ai"));
        assert!(!supports_url_fix("minimax"));
        assert!(!supports_url_fix("dashscope-coding"));
    }

    #[tokio::test]
    async fn load_config_nonexistent_provider_returns_not_found() {
        let (svc, _db) = setup().await;
        let err = svc.load_provider_config("no_such_id").await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn load_config_empty_api_key_returns_bad_request() {
        let (svc, db) = setup().await;
        let id = create_provider(&db, "openai", "https://api.openai.com", "   ").await;
        let err = svc.load_provider_config(&id).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn load_config_decrypts_api_key() {
        let (svc, db) = setup().await;
        let id = create_provider(&db, "openai", "https://api.openai.com", "sk-test-key").await;
        let config = svc.load_provider_config(&id).await.unwrap();
        assert_eq!(config.api_key, "sk-test-key");
        assert_eq!(config.platform, "openai");
        assert_eq!(config.base_url, "https://api.openai.com");
        assert!(config.bedrock_config.is_none());
    }

    #[tokio::test]
    async fn fetch_models_vertex_ai_returns_hardcoded() {
        let (svc, db) = setup().await;
        let id = create_provider(&db, "vertex-ai", "https://unused", "fake-key").await;
        let req = FetchModelsRequest { try_fix: false };
        let resp = svc.fetch_models(&id, &req).await.unwrap();
        assert_eq!(resp.models.len(), 2);
        assert!(resp.fixed_base_url.is_none());
    }

    #[tokio::test]
    async fn fetch_models_minimax_returns_hardcoded() {
        let (svc, db) = setup().await;
        let id = create_provider(&db, "minimax", "https://unused", "fake-key").await;
        let req = FetchModelsRequest { try_fix: false };
        let resp = svc.fetch_models(&id, &req).await.unwrap();
        assert_eq!(resp.models.len(), 3);
    }

    #[tokio::test]
    async fn fetch_models_nonexistent_provider() {
        let (svc, _db) = setup().await;
        let req = FetchModelsRequest { try_fix: false };
        let err = svc.fetch_models("no_such_id", &req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn fetch_models_anonymous_minimax_returns_hardcoded() {
        let (svc, _db) = setup().await;
        let req = FetchModelsAnonymousRequest {
            platform: "minimax".into(),
            base_url: "https://unused".into(),
            api_key: "fake-key".into(),
            bedrock_config: None,
            try_fix: false,
        };
        let resp = svc.fetch_models_anonymous(&req).await.unwrap();
        assert_eq!(resp.models.len(), 3);
        assert!(resp.fixed_base_url.is_none());
    }

    #[tokio::test]
    async fn fetch_models_anonymous_rejects_empty_api_key() {
        let (svc, _db) = setup().await;
        let req = FetchModelsAnonymousRequest {
            platform: "openai".into(),
            base_url: "https://api.openai.com".into(),
            api_key: "   ".into(),
            bedrock_config: None,
            try_fix: false,
        };
        let err = svc.fetch_models_anonymous(&req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn fetch_models_anonymous_rejects_empty_platform() {
        let (svc, _db) = setup().await;
        let req = FetchModelsAnonymousRequest {
            platform: "".into(),
            base_url: "https://api.openai.com".into(),
            api_key: "sk-test".into(),
            bedrock_config: None,
            try_fix: false,
        };
        let err = svc.fetch_models_anonymous(&req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn fetch_models_anonymous_bedrock_allows_empty_api_key() {
        // Bedrock uses bedrock_config for credentials, not api_key.
        // With no bedrock_config attached the fetcher itself will fail,
        // but validate_anonymous_request must not reject up-front.
        let (_svc, _db) = setup().await;
        let req = FetchModelsAnonymousRequest {
            platform: "bedrock".into(),
            base_url: "https://bedrock.example".into(),
            api_key: "".into(),
            bedrock_config: None,
            try_fix: false,
        };
        assert!(validate_anonymous_request(&req).is_ok());
    }
}
