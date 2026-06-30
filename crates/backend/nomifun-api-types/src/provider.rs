use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use nomifun_common::ProtocolType;

/// Model capability type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    Text,
    Vision,
    FunctionCalling,
    ImageGeneration,
    WebSearch,
    Reasoning,
    Embedding,
    Rerank,
    #[serde(rename = "excludeFromPrimary")]
    ExcludeFromPrimary,
}

/// A single model capability entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCapability {
    #[serde(rename = "type")]
    pub capability_type: ModelType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_user_selected: Option<bool>,
}

/// Health status values for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Unknown,
    Healthy,
    Unhealthy,
}

/// Per-model health check information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelHealthStatus {
    pub status: HealthStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Coarse failure category for provider/model health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthCheckErrorKind {
    Timeout,
    InvalidAuthorizationHeader,
    Unauthorized,
    Forbidden,
    NotFound,
    InsufficientQuota,
    AwsCredentials,
    InvalidRequest,
    RateLimited,
    ConnectionError,
    ApiError,
    Unknown,
}

/// Request body for `POST /api/agents/provider-health-check`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderHealthCheckRequest {
    pub provider_id: String,
    pub model: String,
}

/// Response body for `POST /api/agents/provider-health-check`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderHealthCheckResponse {
    pub provider_id: String,
    pub platform: String,
    pub model: String,
    pub status: HealthStatus,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ProviderHealthCheckErrorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_stage: Option<String>,
}

/// AWS Bedrock authentication method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockAuthMethod {
    #[serde(rename = "accessKey")]
    AccessKey,
    Profile,
}

/// AWS Bedrock-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BedrockConfig {
    pub auth_method: BedrockAuthMethod,
    pub region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Provider response for `GET /api/providers` and single-provider endpoints.
///
/// The `api_key` field is returned in plaintext (decrypted on read). Storage
/// remains encrypted at rest. Pre-launch convention for the frontend
/// local-store → backend migration; no masking applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderResponse {
    pub id: String,
    pub platform: String,
    pub name: String,
    pub base_url: String,
    /// Plaintext API key (decrypted from storage).
    pub api_key: String,
    pub models: Vec<String>,
    pub enabled: bool,
    pub capabilities: Vec<ModelCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_protocols: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_descriptions: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_enabled: Option<HashMap<String, bool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_health: Option<HashMap<String, ModelHealthStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bedrock_config: Option<BedrockConfig>,
    #[serde(default)]
    pub is_full_url: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Request body for `POST /api/providers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProviderRequest {
    /// Optional caller-supplied id. When `None`, the server generates one.
    /// Lets callers preserve a locally-known id across the create boundary
    /// (used during the frontend-local-store → backend migration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub platform: String,
    pub name: String,
    pub base_url: String,
    /// Plain-text API key (supports comma/newline-separated multi-keys).
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub capabilities: Vec<ModelCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_protocols: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_descriptions: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_enabled: Option<HashMap<String, bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_health: Option<HashMap<String, ModelHealthStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_config: Option<BedrockConfig>,
    #[serde(default)]
    pub is_full_url: bool,
}

fn default_true() -> bool {
    true
}

/// Request body for `PUT /api/providers/:id`.
///
/// All fields are optional — partial update semantics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProviderRequest {
    pub platform: Option<String>,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub models: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub capabilities: Option<Vec<ModelCapability>>,
    pub context_limit: Option<i64>,
    pub model_protocols: Option<HashMap<String, String>>,
    pub model_descriptions: Option<HashMap<String, String>>,
    pub model_enabled: Option<HashMap<String, bool>>,
    pub model_health: Option<HashMap<String, ModelHealthStatus>>,
    pub bedrock_config: Option<BedrockConfig>,
    pub is_full_url: Option<bool>,
}

/// Request body for `POST /api/providers/:id/models`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FetchModelsRequest {
    #[serde(default)]
    pub try_fix: bool,
}

/// Request body for `POST /api/providers/fetch-models` (anonymous, pre-create).
///
/// Used by the frontend's Add-Platform form to preview a provider's model
/// list before the provider row is persisted — credentials are passed in
/// the request body instead of looked up by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchModelsAnonymousRequest {
    pub platform: String,
    pub base_url: String,
    /// Plain-text API key (supports multi-key).
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_config: Option<BedrockConfig>,
    #[serde(default)]
    pub try_fix: bool,
}

/// A model entry that can be either a bare ID string or an object with
/// id and name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ModelInfo {
    Id(String),
    Named { id: String, name: String },
}

/// Response for `POST /api/providers/:id/models`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FetchModelsResponse {
    pub models: Vec<ModelInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_base_url: Option<String>,
}

/// Request body for `POST /api/providers/detect-protocol`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectProtocolRequest {
    pub base_url: String,
    /// Plain-text API key (supports multi-key).
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub test_all_keys: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_protocol: Option<ProtocolType>,
}

/// Suggestion type for protocol detection results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionType {
    None,
    CheckKey,
    SwitchPlatform,
}

/// Actionable suggestion from protocol detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectionSuggestion {
    #[serde(rename = "type")]
    pub suggestion_type: SuggestionType,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub i18n_key: Option<String>,
}

/// Per-key test result in multi-key protocol detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyTestResult {
    pub index: usize,
    pub masked_key: String,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated result of testing multiple API keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MultiKeyResult {
    pub total: usize,
    pub valid: usize,
    pub invalid: usize,
    pub details: Vec<KeyTestResult>,
}

/// A single detected protocol entry in multi-protocol detection results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectedProtocol {
    pub protocol: ProtocolType,
    pub confidence: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

/// Response for `POST /api/providers/detect-protocol`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProtocolDetectionResponse {
    pub protocol: ProtocolType,
    pub confidence: u8,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<DetectionSuggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_key_result: Option<MultiKeyResult>,
    #[serde(default, rename = "detectedProtocols", skip_serializing_if = "Vec::is_empty")]
    pub detected_protocols: Vec<DetectedProtocol>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- ModelType --

    #[test]
    fn test_model_type_serialization() {
        assert_eq!(serde_json::to_string(&ModelType::Text).unwrap(), r#""text""#);
        assert_eq!(
            serde_json::to_string(&ModelType::FunctionCalling).unwrap(),
            r#""function_calling""#
        );
        assert_eq!(
            serde_json::to_string(&ModelType::ExcludeFromPrimary).unwrap(),
            r#""excludeFromPrimary""#
        );
    }

    #[test]
    fn test_model_type_roundtrip() {
        for mt in [
            ModelType::Text,
            ModelType::Vision,
            ModelType::FunctionCalling,
            ModelType::ImageGeneration,
            ModelType::WebSearch,
            ModelType::Reasoning,
            ModelType::Embedding,
            ModelType::Rerank,
            ModelType::ExcludeFromPrimary,
        ] {
            let json = serde_json::to_string(&mt).unwrap();
            let parsed: ModelType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mt);
        }
    }

    // -- ModelCapability --

    #[test]
    fn test_model_capability_serialization() {
        let cap = ModelCapability {
            capability_type: ModelType::Vision,
            is_user_selected: Some(true),
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert_eq!(json["type"], "vision");
        assert_eq!(json["is_user_selected"], true);
    }

    #[test]
    fn test_model_capability_optional_field_skipped() {
        let cap = ModelCapability {
            capability_type: ModelType::Text,
            is_user_selected: None,
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert_eq!(json["type"], "text");
        assert!(json.get("is_user_selected").is_none());
    }

    // -- HealthStatus / ModelHealthStatus --

    #[test]
    fn test_health_status_serialization() {
        assert_eq!(serde_json::to_string(&HealthStatus::Healthy).unwrap(), r#""healthy""#);
        assert_eq!(
            serde_json::to_string(&HealthStatus::Unhealthy).unwrap(),
            r#""unhealthy""#
        );
        assert_eq!(serde_json::to_string(&HealthStatus::Unknown).unwrap(), r#""unknown""#);
    }

    #[test]
    fn test_model_health_status_full() {
        let status = ModelHealthStatus {
            status: HealthStatus::Healthy,
            last_check: Some(1712345678000),
            latency: Some(320),
            error: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["status"], "healthy");
        assert_eq!(json["last_check"], 1712345678000_i64);
        assert_eq!(json["latency"], 320);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_model_health_status_minimal() {
        let status = ModelHealthStatus {
            status: HealthStatus::Unknown,
            last_check: None,
            latency: None,
            error: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["status"], "unknown");
        assert!(json.get("last_check").is_none());
    }

    // -- BedrockConfig --

    #[test]
    fn test_bedrock_config_access_key() {
        let cfg = BedrockConfig {
            auth_method: BedrockAuthMethod::AccessKey,
            region: "us-east-1".into(),
            access_key_id: Some("AKIAIOSFODNN7".into()),
            secret_access_key: Some("wJalrXUtnFEMI/K7MDENG".into()),
            profile: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["auth_method"], "accessKey");
        assert_eq!(json["region"], "us-east-1");
        assert_eq!(json["access_key_id"], "AKIAIOSFODNN7");
        assert!(json.get("profile").is_none());
    }

    #[test]
    fn test_bedrock_config_profile() {
        let cfg = BedrockConfig {
            auth_method: BedrockAuthMethod::Profile,
            region: "eu-west-1".into(),
            access_key_id: None,
            secret_access_key: None,
            profile: Some("my-profile".into()),
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["auth_method"], "profile");
        assert_eq!(json["profile"], "my-profile");
        assert!(json.get("access_key_id").is_none());
    }

    // -- ProviderResponse --

    #[test]
    fn test_provider_response_serialization() {
        let resp = ProviderResponse {
            id: "uuid-xxx".into(),
            platform: "anthropic".into(),
            name: "Anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "sk-ant-api03-plaintext".into(),
            models: vec!["claude-sonnet-4-20250514".into()],
            enabled: true,
            capabilities: vec![ModelCapability {
                capability_type: ModelType::Text,
                is_user_selected: None,
            }],
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: Some(HashMap::from([("claude-sonnet-4-20250514".into(), true)])),
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 1712345678000,
            updated_at: 1712345678000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "uuid-xxx");
        assert_eq!(json["platform"], "anthropic");
        assert_eq!(json["api_key"], "sk-ant-api03-plaintext");
        assert_eq!(json["base_url"], "https://api.anthropic.com");
        assert_eq!(json["models"][0], "claude-sonnet-4-20250514");
        assert_eq!(json["model_enabled"]["claude-sonnet-4-20250514"], true);
        assert!(json.get("context_limit").is_none());
        assert!(json.get("model_protocols").is_none());
        assert!(json.get("bedrock_config").is_none());
    }

    #[test]
    fn test_provider_response_api_key_plaintext() {
        // Pre-launch: no masking is applied to the api_key field on the wire.
        let resp = ProviderResponse {
            id: "id".into(),
            platform: "openai".into(),
            name: "n".into(),
            base_url: "https://api.openai.com".into(),
            api_key: "sk-secret-xyz".into(),
            models: vec![],
            enabled: true,
            capabilities: vec![],
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
            created_at: 0,
            updated_at: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_key"], "sk-secret-xyz");
        assert!(!json["api_key"].as_str().unwrap().contains("***"));
    }

    // -- CreateProviderRequest --

    #[test]
    fn test_create_provider_request_required_fields() {
        let raw = json!({
            "platform": "anthropic",
            "name": "Anthropic",
            "base_url": "https://api.anthropic.com",
            "api_key": "sk-ant-api03-test"
        });
        let req: CreateProviderRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.platform, "anthropic");
        assert_eq!(req.name, "Anthropic");
        assert_eq!(req.base_url, "https://api.anthropic.com");
        assert_eq!(req.api_key, "sk-ant-api03-test");
        assert!(req.models.is_empty());
        assert!(req.enabled);
        assert!(req.capabilities.is_empty());
        assert!(req.context_limit.is_none());
        assert!(req.bedrock_config.is_none());
    }

    #[test]
    fn test_create_provider_request_missing_required_field() {
        let raw = json!({"platform": "anthropic", "name": "Anthropic"});
        let result = serde_json::from_value::<CreateProviderRequest>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_provider_request_with_optional_fields() {
        let raw = json!({
            "platform": "bedrock",
            "name": "AWS Bedrock",
            "base_url": "https://bedrock.us-east-1.amazonaws.com",
            "api_key": "",
            "models": ["anthropic.claude-3-sonnet"],
            "enabled": false,
            "capabilities": [{"type": "text"}, {"type": "vision", "is_user_selected": true}],
            "context_limit": 200000,
            "bedrock_config": {
                "auth_method": "accessKey",
                "region": "us-east-1",
                "access_key_id": "AKIA...",
                "secret_access_key": "secret"
            }
        });
        let req: CreateProviderRequest = serde_json::from_value(raw).unwrap();
        assert!(req.id.is_none());
        assert!(!req.enabled);
        assert_eq!(req.models.len(), 1);
        assert_eq!(req.capabilities.len(), 2);
        assert_eq!(req.context_limit, Some(200000));
        assert!(req.bedrock_config.is_some());
    }

    #[test]
    fn test_create_provider_request_with_id() {
        let raw = json!({
            "id": "caller-supplied-1",
            "platform": "openai",
            "name": "OpenAI",
            "base_url": "https://api.openai.com",
            "api_key": "sk-test"
        });
        let req: CreateProviderRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.id.as_deref(), Some("caller-supplied-1"));
    }

    #[test]
    fn test_create_provider_request_with_per_model_fields() {
        let raw = json!({
            "platform": "openai",
            "name": "OpenAI",
            "base_url": "https://api.openai.com",
            "api_key": "sk-test",
            "models": ["gpt-4", "gpt-3.5"],
            "model_protocols": {"gpt-4": "openai"},
            "model_enabled": {"gpt-4": true, "gpt-3.5": false},
            "model_health": {
                "gpt-4": {"status": "healthy", "last_check": 1712345678000_i64, "latency": 320}
            }
        });
        let req: CreateProviderRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(
            req.model_protocols.as_ref().unwrap().get("gpt-4"),
            Some(&"openai".to_string())
        );
        assert_eq!(req.model_enabled.as_ref().unwrap().get("gpt-4"), Some(&true));
        assert_eq!(req.model_enabled.as_ref().unwrap().get("gpt-3.5"), Some(&false));
        let health = req.model_health.as_ref().unwrap().get("gpt-4").unwrap();
        assert_eq!(health.status, HealthStatus::Healthy);
        assert_eq!(health.latency, Some(320));
    }

    #[test]
    fn test_create_provider_request_id_skipped_when_none() {
        // When id is None, it should not appear in serialized output.
        let req = CreateProviderRequest {
            id: None,
            platform: "openai".into(),
            name: "OpenAI".into(),
            base_url: "https://api.openai.com".into(),
            api_key: "sk-test".into(),
            models: vec![],
            enabled: true,
            capabilities: vec![],
            context_limit: None,
            model_protocols: None,
            model_descriptions: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: false,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("id").is_none());
    }

    // -- UpdateProviderRequest --

    #[test]
    fn test_update_provider_request_partial() {
        let raw = json!({"name": "New Name"});
        let req: UpdateProviderRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("New Name"));
        assert!(req.platform.is_none());
        assert!(req.api_key.is_none());
    }

    #[test]
    fn test_update_provider_request_empty() {
        let raw = json!({});
        let req: UpdateProviderRequest = serde_json::from_value(raw).unwrap();
        assert!(req.platform.is_none());
        assert!(req.name.is_none());
    }

    // -- FetchModelsRequest --

    #[test]
    fn test_fetch_models_request_default() {
        let raw = json!({});
        let req: FetchModelsRequest = serde_json::from_value(raw).unwrap();
        assert!(!req.try_fix);
    }

    #[test]
    fn test_fetch_models_request_with_try_fix() {
        let raw = json!({"try_fix": true});
        let req: FetchModelsRequest = serde_json::from_value(raw).unwrap();
        assert!(req.try_fix);
    }

    // -- FetchModelsAnonymousRequest --

    #[test]
    fn test_fetch_models_anonymous_request_required_fields() {
        let raw = json!({
            "platform": "openai",
            "base_url": "https://api.openai.com",
            "api_key": "sk-test"
        });
        let req: FetchModelsAnonymousRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.platform, "openai");
        assert_eq!(req.base_url, "https://api.openai.com");
        assert_eq!(req.api_key, "sk-test");
        assert!(req.bedrock_config.is_none());
        assert!(!req.try_fix);
    }

    #[test]
    fn test_fetch_models_anonymous_request_with_bedrock() {
        let raw = json!({
            "platform": "bedrock",
            "base_url": "https://bedrock.us-east-1.amazonaws.com",
            "api_key": "",
            "bedrock_config": {
                "auth_method": "accessKey",
                "region": "us-east-1",
                "access_key_id": "AKIA...",
                "secret_access_key": "secret"
            },
            "try_fix": true
        });
        let req: FetchModelsAnonymousRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.platform, "bedrock");
        assert!(req.try_fix);
        let cfg = req.bedrock_config.unwrap();
        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(cfg.auth_method, BedrockAuthMethod::AccessKey);
    }

    #[test]
    fn test_fetch_models_anonymous_request_missing_required_field() {
        let raw = json!({"platform": "openai", "api_key": "sk"});
        assert!(serde_json::from_value::<FetchModelsAnonymousRequest>(raw).is_err());
    }

    // -- ModelInfo --

    #[test]
    fn test_model_info_string() {
        let info: ModelInfo = serde_json::from_value(json!("gpt-4")).unwrap();
        assert_eq!(info, ModelInfo::Id("gpt-4".into()));
    }

    #[test]
    fn test_model_info_named() {
        let info: ModelInfo = serde_json::from_value(json!({"id": "gpt-4", "name": "GPT-4"})).unwrap();
        assert_eq!(
            info,
            ModelInfo::Named {
                id: "gpt-4".into(),
                name: "GPT-4".into()
            }
        );
    }

    #[test]
    fn test_model_info_mixed_array() {
        let raw = json!(["gpt-4", {"id": "claude-3", "name": "Claude 3"}]);
        let models: Vec<ModelInfo> = serde_json::from_value(raw).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], ModelInfo::Id("gpt-4".into()));
    }

    // -- FetchModelsResponse --

    #[test]
    fn test_fetch_models_response_without_fixed_url() {
        let resp = FetchModelsResponse {
            models: vec![
                ModelInfo::Id("claude-sonnet-4-20250514".into()),
                ModelInfo::Id("claude-opus-4-20250514".into()),
            ],
            fixed_base_url: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["models"].as_array().unwrap().len(), 2);
        assert!(json.get("fixed_base_url").is_none());
    }

    #[test]
    fn test_fetch_models_response_with_fixed_url() {
        let resp = FetchModelsResponse {
            models: vec![ModelInfo::Id("gpt-4".into())],
            fixed_base_url: Some("https://api.openai.com/v1".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["fixed_base_url"], "https://api.openai.com/v1");
    }

    // -- DetectProtocolRequest --

    #[test]
    fn test_detect_protocol_request_required_only() {
        let raw = json!({
            "base_url": "https://api.example.com",
            "api_key": "sk-xxx"
        });
        let req: DetectProtocolRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.base_url, "https://api.example.com");
        assert_eq!(req.api_key, "sk-xxx");
        assert!(req.timeout.is_none());
        assert!(!req.test_all_keys);
        assert!(req.preferred_protocol.is_none());
    }

    #[test]
    fn test_detect_protocol_request_full() {
        let raw = json!({
            "base_url": "https://api.anthropic.com",
            "api_key": "sk-ant-xxx",
            "timeout": 10000,
            "test_all_keys": true,
            "preferred_protocol": "anthropic"
        });
        let req: DetectProtocolRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.timeout, Some(10000));
        assert!(req.test_all_keys);
        assert_eq!(req.preferred_protocol, Some(ProtocolType::Anthropic));
    }

    // -- SuggestionType --

    #[test]
    fn test_suggestion_type_serialization() {
        assert_eq!(serde_json::to_string(&SuggestionType::None).unwrap(), r#""none""#);
        assert_eq!(
            serde_json::to_string(&SuggestionType::CheckKey).unwrap(),
            r#""check_key""#
        );
        assert_eq!(
            serde_json::to_string(&SuggestionType::SwitchPlatform).unwrap(),
            r#""switch_platform""#
        );
    }

    // -- ProtocolDetectionResponse --

    #[test]
    fn test_protocol_detection_response_minimal() {
        let resp = ProtocolDetectionResponse {
            protocol: ProtocolType::Unknown,
            confidence: 0,
            success: false,
            fixed_base_url: None,
            models: None,
            suggestion: None,
            multi_key_result: None,
            detected_protocols: vec![],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["protocol"], "unknown");
        assert_eq!(json["confidence"], 0);
        assert_eq!(json["success"], false);
        assert!(json.get("fixed_base_url").is_none());
        assert!(json.get("models").is_none());
        assert!(json.get("suggestion").is_none());
        assert!(json.get("multi_key_result").is_none());
        assert!(json.get("detectedProtocols").is_none());
    }

    #[test]
    fn test_protocol_detection_response_full() {
        let resp = ProtocolDetectionResponse {
            protocol: ProtocolType::Anthropic,
            confidence: 95,
            success: true,
            fixed_base_url: None,
            models: Some(vec!["claude-sonnet-4-20250514".into()]),
            suggestion: Some(DetectionSuggestion {
                suggestion_type: SuggestionType::None,
                message: "Detected Anthropic protocol".into(),
                i18n_key: Some("settings.protocolDetected".into()),
            }),
            multi_key_result: None,
            detected_protocols: vec![DetectedProtocol {
                protocol: ProtocolType::Anthropic,
                confidence: 95,
                models: Some(vec!["claude-sonnet-4-20250514".into()]),
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["protocol"], "anthropic");
        assert_eq!(json["confidence"], 95);
        assert_eq!(json["success"], true);
        assert_eq!(json["suggestion"]["type"], "none");
        assert_eq!(json["suggestion"]["message"], "Detected Anthropic protocol");
        assert_eq!(json["suggestion"]["i18n_key"], "settings.protocolDetected");
        let dp = json["detectedProtocols"].as_array().unwrap();
        assert_eq!(dp.len(), 1);
        assert_eq!(dp[0]["protocol"], "anthropic");
        assert_eq!(dp[0]["confidence"], 95);
    }

    #[test]
    fn test_protocol_detection_response_multi_key() {
        let resp = ProtocolDetectionResponse {
            protocol: ProtocolType::OpenAI,
            confidence: 90,
            success: true,
            fixed_base_url: None,
            models: None,
            suggestion: None,
            multi_key_result: Some(MultiKeyResult {
                total: 3,
                valid: 2,
                invalid: 1,
                details: vec![
                    KeyTestResult {
                        index: 0,
                        masked_key: "sk-***abcd".into(),
                        valid: true,
                        latency: Some(320),
                        error: None,
                    },
                    KeyTestResult {
                        index: 1,
                        masked_key: "sk-***efgh".into(),
                        valid: true,
                        latency: Some(280),
                        error: None,
                    },
                    KeyTestResult {
                        index: 2,
                        masked_key: "sk-***ijkl".into(),
                        valid: false,
                        latency: Some(150),
                        error: Some("Invalid API key".into()),
                    },
                ],
            }),
            detected_protocols: vec![],
        };
        let json = serde_json::to_value(&resp).unwrap();
        let mkr = &json["multi_key_result"];
        assert_eq!(mkr["total"], 3);
        assert_eq!(mkr["valid"], 2);
        assert_eq!(mkr["invalid"], 1);
        assert_eq!(mkr["details"].as_array().unwrap().len(), 3);
        assert_eq!(mkr["details"][0]["masked_key"], "sk-***abcd");
        assert_eq!(mkr["details"][2]["error"], "Invalid API key");
    }

    // -- is_full_url --

    #[test]
    fn test_create_provider_request_with_is_full_url() {
        let raw = json!({
            "platform": "custom",
            "name": "Custom Proxy",
            "base_url": "https://proxy.example.com/v1/chat/completions",
            "api_key": "sk-test",
            "is_full_url": true
        });
        let req: CreateProviderRequest = serde_json::from_value(raw).unwrap();
        assert!(req.is_full_url);
    }

    #[test]
    fn test_create_provider_request_is_full_url_defaults_false() {
        let raw = json!({
            "platform": "openai",
            "name": "OpenAI",
            "base_url": "https://api.openai.com",
            "api_key": "sk-test"
        });
        let req: CreateProviderRequest = serde_json::from_value(raw).unwrap();
        assert!(!req.is_full_url);
    }

    #[test]
    fn test_provider_health_check_request_serde() {
        let raw = json!({
            "provider_id": "anthropic",
            "model": "claude-sonnet-4-20250514"
        });
        let req: ProviderHealthCheckRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.provider_id, "anthropic");
        assert_eq!(req.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_provider_health_check_response_serde() {
        let resp = ProviderHealthCheckResponse {
            provider_id: "anthropic".into(),
            platform: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            status: HealthStatus::Unhealthy,
            elapsed_ms: 1234,
            message: Some("API error 401: invalid key".into()),
            error_kind: Some(ProviderHealthCheckErrorKind::Unauthorized),
            http_status: Some(401),
            timeout_stage: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["provider_id"], "anthropic");
        assert_eq!(json["status"], "unhealthy");
        assert_eq!(json["error_kind"], "unauthorized");
        assert_eq!(json["http_status"], 401);
        assert!(json.get("timeout_stage").is_none());
    }
}
