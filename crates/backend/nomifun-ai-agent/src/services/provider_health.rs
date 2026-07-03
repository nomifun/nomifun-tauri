use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nomi_agent::bootstrap::AgentBootstrap;
use nomi_agent::engine::AgentEngine;
use nomi_agent::output::OutputSink;
use nomi_agent::output::null_sink::NullSink;
use nomi_config::config::{CliArgs, Config};
use nomifun_api_types::{
    HealthStatus, ProviderHealthCheckErrorKind, ProviderHealthCheckRequest,
    ProviderHealthCheckResponse,
};
use nomifun_common::AppError;
use nomifun_db::{IProviderRepository, models::Provider};
use regex::Regex;
use tracing::{info, warn};

use crate::factory::nomi::{
    map_nomi_provider, resolve_bedrock_config, resolve_nomi_url_and_compat,
};
use crate::types::NomiResolvedConfig;

const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const OPENAI_MODEL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";
const HEALTH_CHECK_PROMPT: &str = "Reply with exactly OK.";
const HEALTH_CHECK_MSG_ID: &str = "provider-health-check";

pub struct ProviderHealthCheckService {
    provider_repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    data_dir: PathBuf,
}

impl ProviderHealthCheckService {
    pub fn new(
        provider_repo: Arc<dyn IProviderRepository>,
        encryption_key: [u8; 32],
        data_dir: PathBuf,
    ) -> Self {
        Self {
            provider_repo,
            encryption_key,
            data_dir,
        }
    }

    pub async fn health_check(
        &self,
        req: ProviderHealthCheckRequest,
    ) -> Result<ProviderHealthCheckResponse, AppError> {
        if req.provider_id.trim().is_empty() {
            return Err(AppError::BadRequest("provider_id is required".into()));
        }
        if req.model.trim().is_empty() {
            return Err(AppError::BadRequest("model is required".into()));
        }

        let provider_id = req.provider_id.trim();
        let model = req.model.trim();
        let row = self
            .provider_repo
            .find_by_id(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to load provider config: {e}")))?
            .ok_or_else(|| AppError::BadRequest(format!("Provider '{provider_id}' not found")))?;

        let config = self.resolve_probe_config(&row, model)?;
        if should_use_openai_model_probe(&row.platform, &config) {
            return run_openai_model_probe(
                row.id,
                row.platform,
                model.to_owned(),
                config.api_key,
                config.base_url,
            )
            .await;
        }

        run_probe(row.id, row.platform, config).await
    }

    fn resolve_probe_config(
        &self,
        row: &Provider,
        model_id: &str,
    ) -> Result<NomiResolvedConfig, AppError> {
        let api_key = nomifun_common::decrypt_string(&row.api_key_encrypted, &self.encryption_key)?;
        let provider = map_nomi_provider(&row.platform, model_id, row.model_protocols.as_deref());
        let (base_url, compat_overrides) =
            resolve_nomi_url_and_compat(&row.platform, &row.base_url, &provider, row.is_full_url);
        let bedrock_config = if row.platform == "bedrock" {
            resolve_bedrock_config(row.bedrock_config.as_deref())
        } else {
            None
        };

        Ok(NomiResolvedConfig {
            provider,
            api_key,
            model: model_id.to_owned(),
            base_url,
            system_prompt: Some(
                "You are a provider health probe. Reply with exactly OK and do not use tools."
                    .into(),
            ),
            max_tokens: 16,
            max_turns: Some(1),
            context_limit: None,
            compat_overrides,
            session_directory: self.data_dir.join("nomi-health-check-sessions"),
            session_mode: None,
            extra_mcp_servers: HashMap::new(),
            bedrock_config,
            computer_use: false,
            browser_use: false,
            browser_silent: true,
            browser_source: "managed".to_owned(),
            browser_full_power: false,
            browser_persistent_login: false,
            browser_site_memory: false,
            browser_takeover: false,
            browser_visual_fallback: false,
            goal: None,
            browser_secret_vault: None,
            owner_token: None,
            // 健康探针一回合、不用工具：不必构造进程内 Spawn。
            in_process_spawn: false,
            allowed_tools: Vec::new(),
            write_root: None,
        })
    }
}

fn should_use_openai_model_probe(_platform: &str, config: &NomiResolvedConfig) -> bool {
    config.provider == "openai"
        && config
            .base_url
            .as_deref()
            .map(is_official_openai_base_url)
            .unwrap_or(true)
}

fn is_official_openai_base_url(base_url: &str) -> bool {
    let lower = base_url.trim().to_lowercase();
    let without_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    without_scheme == "api.openai.com" || without_scheme.starts_with("api.openai.com/")
}

async fn run_openai_model_probe(
    provider_id: String,
    platform: String,
    model: String,
    api_key: String,
    base_url: Option<String>,
) -> Result<ProviderHealthCheckResponse, AppError> {
    let started = Instant::now();
    let url = openai_model_probe_url(base_url.as_deref(), &model);
    let client = nomifun_net::http_client();

    info!(
        provider_id = %provider_id,
        platform = %platform,
        model = %model,
        "OpenAI model health check started"
    );

    match tokio::time::timeout(
        OPENAI_MODEL_PROBE_TIMEOUT,
        client.get(&url).bearer_auth(api_key).send(),
    )
    .await
    {
        Ok(Ok(response)) if response.status().is_success() => {
            let response = ProviderHealthCheckResponse {
                provider_id,
                platform,
                model,
                status: HealthStatus::Healthy,
                elapsed_ms: elapsed_ms(started.elapsed()),
                message: None,
                error_kind: None,
                http_status: None,
                timeout_stage: None,
            };
            log_health_check_result(&response);
            Ok(response)
        }
        Ok(Ok(response)) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let message = format!("OpenAI model probe API error {}: {body}", status.as_u16());
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                message,
                None,
            );
            log_health_check_result(&response);
            Ok(response)
        }
        Ok(Err(error)) => {
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                format!("OpenAI model probe HTTP error: {error}"),
                None,
            );
            log_health_check_result(&response);
            Ok(response)
        }
        Err(_) => {
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                format!(
                    "OpenAI model probe timeout ({}s)",
                    OPENAI_MODEL_PROBE_TIMEOUT.as_secs()
                ),
                Some("openai_models".into()),
            );
            log_health_check_result(&response);
            Ok(response)
        }
    }
}

fn openai_model_probe_url(base_url: Option<&str>, model: &str) -> String {
    let base = base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_OPENAI_BASE_URL)
        .trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/models/{model}")
}

async fn run_probe(
    provider_id: String,
    platform: String,
    config_extra: NomiResolvedConfig,
) -> Result<ProviderHealthCheckResponse, AppError> {
    let started = Instant::now();
    let model = config_extra.model.clone();

    info!(
        provider_id = %provider_id,
        platform = %platform,
        model = %model,
        "Provider health check started"
    );

    let mut engine = match build_probe_engine(config_extra).await {
        Ok(engine) => engine,
        Err(error) => {
            let message = format!("Nomi probe bootstrap failed: {error}");
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                message,
                None,
            );
            log_health_check_result(&response);
            return Ok(response);
        }
    };

    match tokio::time::timeout(
        HEALTH_CHECK_TIMEOUT,
        engine.run(HEALTH_CHECK_PROMPT, HEALTH_CHECK_MSG_ID),
    )
    .await
    {
        Ok(Ok(_)) => {
            let response = ProviderHealthCheckResponse {
                provider_id,
                platform,
                model,
                status: HealthStatus::Healthy,
                elapsed_ms: elapsed_ms(started.elapsed()),
                message: None,
                error_kind: None,
                http_status: None,
                timeout_stage: None,
            };
            log_health_check_result(&response);
            Ok(response)
        }
        Ok(Err(error)) => {
            let message = error.to_string();
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                message,
                None,
            );
            log_health_check_result(&response);
            Ok(response)
        }
        Err(_) => {
            let response = unhealthy_response(
                provider_id,
                platform,
                model,
                started.elapsed(),
                format!("Health check timeout ({}s)", HEALTH_CHECK_TIMEOUT.as_secs()),
                Some("engine_run".into()),
            );
            log_health_check_result(&response);
            Ok(response)
        }
    }
}

fn log_health_check_result(response: &ProviderHealthCheckResponse) {
    match response.status {
        HealthStatus::Healthy => info!(
            provider_id = %response.provider_id,
            platform = %response.platform,
            model = %response.model,
            elapsed_ms = response.elapsed_ms,
            "Provider health check succeeded"
        ),
        HealthStatus::Unhealthy | HealthStatus::Unknown => warn!(
            provider_id = %response.provider_id,
            platform = %response.platform,
            model = %response.model,
            elapsed_ms = response.elapsed_ms,
            error_kind = ?response.error_kind,
            http_status = ?response.http_status,
            timeout_stage = ?response.timeout_stage,
            "Provider health check failed"
        ),
    }
}

async fn build_probe_engine(config_extra: NomiResolvedConfig) -> Result<AgentEngine, AppError> {
    let workspace = config_extra
        .session_directory
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sink: Arc<dyn OutputSink> = Arc::new(NullSink);
    let cli_args = CliArgs {
        provider: Some(config_extra.provider),
        api_key: Some(config_extra.api_key),
        base_url: config_extra.base_url,
        model: Some(config_extra.model),
        max_tokens: Some(config_extra.max_tokens),
        max_turns: config_extra.max_turns,
        system_prompt: config_extra.system_prompt,
        profile: None,
        auto_approve: false,
        project_dir: Some(PathBuf::from(&workspace)),
    };
    let mut config = Config::resolve(&cli_args)
        .map_err(|error| AppError::Internal(format!("Config resolve failed: {error}")))?;

    config.bedrock = config_extra.bedrock_config;
    config.session.enabled = false;
    config.mcp.servers.clear();
    config.file_cache.enabled = false;
    if let Some(field) = config_extra.compat_overrides.max_tokens_field {
        config.compat.max_tokens_field = Some(field);
    }
    if let Some(path) = config_extra.compat_overrides.api_path {
        config.compat.api_path = Some(path);
    }

    AgentBootstrap::new(config, workspace, sink)
        .build()
        .await
        .map(|result| result.engine)
        .map_err(|error| AppError::Internal(error.to_string()))
}

fn unhealthy_response(
    provider_id: String,
    platform: String,
    model: String,
    elapsed: Duration,
    message: String,
    timeout_stage: Option<String>,
) -> ProviderHealthCheckResponse {
    let error_kind = classify_error(&message, timeout_stage.is_some());
    let http_status = extract_http_status(&message);
    ProviderHealthCheckResponse {
        provider_id,
        platform,
        model,
        status: HealthStatus::Unhealthy,
        elapsed_ms: elapsed_ms(elapsed),
        message: Some(message),
        error_kind: Some(error_kind),
        http_status,
        timeout_stage,
    }
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub(crate) fn classify_error(message: &str, is_timeout: bool) -> ProviderHealthCheckErrorKind {
    if is_timeout {
        return ProviderHealthCheckErrorKind::Timeout;
    }

    let lower = message.to_lowercase();
    if lower.contains("invalid authorization header") || lower.contains("invalid x-api-key header")
    {
        return ProviderHealthCheckErrorKind::InvalidAuthorizationHeader;
    }
    if lower.contains("rate limited") || lower.contains(" 429") || lower.contains("api error 429") {
        return ProviderHealthCheckErrorKind::RateLimited;
    }
    if lower.contains("insufficient_quota")
        || lower.contains("insufficient quota")
        || lower.contains("credit balance is too low")
        || lower.contains("billing")
    {
        return ProviderHealthCheckErrorKind::InsufficientQuota;
    }
    if lower.contains("aws credential")
        || lower.contains("loading credentials")
        || lower.contains("invalid refresh token")
        || lower.contains("session token not found")
    {
        return ProviderHealthCheckErrorKind::AwsCredentials;
    }
    if lower.contains("api error 401")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
    {
        return ProviderHealthCheckErrorKind::Unauthorized;
    }
    if lower.contains("api error 403") || lower.contains("forbidden") {
        return ProviderHealthCheckErrorKind::Forbidden;
    }
    if lower.contains("api error 404") || lower.contains("not found") {
        return ProviderHealthCheckErrorKind::NotFound;
    }
    if lower.contains("api error 400")
        || lower.contains("invalid_request")
        || lower.contains("invalid request")
    {
        return ProviderHealthCheckErrorKind::InvalidRequest;
    }
    if lower.contains("connection error") || lower.contains("http error") {
        return ProviderHealthCheckErrorKind::ConnectionError;
    }
    if lower.contains("api error") || lower.contains("provider error") {
        return ProviderHealthCheckErrorKind::ApiError;
    }

    ProviderHealthCheckErrorKind::Unknown
}

pub(crate) fn extract_http_status(message: &str) -> Option<u16> {
    let re = Regex::new(r"(?i)api error\s+(\d{3})").ok()?;
    re.captures(message)
        .and_then(|captures| captures.get(1))
        .and_then(|matched| matched.as_str().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_detects_quota_message() {
        let message = r#"Provider error: API error 400: {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low"}}"#;
        assert_eq!(
            classify_error(message, false),
            ProviderHealthCheckErrorKind::InsufficientQuota
        );
        assert_eq!(extract_http_status(message), Some(400));
    }

    #[test]
    fn classify_error_detects_invalid_header() {
        assert_eq!(
            classify_error(
                "Connection error: Invalid authorization header: invalid header value",
                false
            ),
            ProviderHealthCheckErrorKind::InvalidAuthorizationHeader
        );
    }

    #[test]
    fn classify_error_detects_aws_credentials() {
        assert_eq!(
            classify_error(
                "Provider error: Connection error: AWS credential error: an error occurred while loading credentials",
                false
            ),
            ProviderHealthCheckErrorKind::AwsCredentials
        );
        assert_eq!(
            classify_error(
                "service error: UnauthorizedException: Session token not found or invalid",
                false
            ),
            ProviderHealthCheckErrorKind::AwsCredentials
        );
    }

    #[test]
    fn classify_error_detects_timeout() {
        assert_eq!(
            classify_error("Health check timeout (30s)", true),
            ProviderHealthCheckErrorKind::Timeout
        );
    }

    #[test]
    fn openai_model_probe_is_used_for_custom_openai_compatible_configs() {
        let config = NomiResolvedConfig {
            provider: "openai".to_owned(),
            api_key: "sk-test".to_owned(),
            model: "gpt-test".to_owned(),
            base_url: Some("https://api.openai.com".to_owned()),
            system_prompt: None,
            max_tokens: 16,
            max_turns: Some(1),
            context_limit: None,
            compat_overrides: crate::types::NomiCompatOverrides::default(),
            session_directory: PathBuf::from("/tmp/nomi-health"),
            session_mode: None,
            extra_mcp_servers: HashMap::new(),
            bedrock_config: None,
            computer_use: false,
            browser_use: false,
            browser_silent: true,
            browser_source: "managed".to_owned(),
            browser_full_power: false,
            browser_persistent_login: false,
            browser_site_memory: false,
            browser_takeover: false,
            browser_visual_fallback: false,
            goal: None,
            browser_secret_vault: None,
            owner_token: None,
            in_process_spawn: false,
            allowed_tools: Vec::new(),
            write_root: None,
        };

        assert!(should_use_openai_model_probe("custom", &config));
    }

    #[tokio::test]
    async fn openai_model_probe_uses_models_endpoint_for_success() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/gpt-test"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gpt-test",
                "object": "model"
            })))
            .mount(&server)
            .await;

        let response = run_openai_model_probe(
            "provider-1".to_owned(),
            "openai".to_owned(),
            "gpt-test".to_owned(),
            "sk-test".to_owned(),
            Some(server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(response.status, HealthStatus::Healthy);
        assert_eq!(response.error_kind, None);
    }

    #[tokio::test]
    async fn openai_model_probe_preserves_rate_limit_classification() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/gpt-test"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let response = run_openai_model_probe(
            "provider-1".to_owned(),
            "openai".to_owned(),
            "gpt-test".to_owned(),
            "sk-test".to_owned(),
            Some(server.uri()),
        )
        .await
        .unwrap();

        assert_eq!(response.status, HealthStatus::Unhealthy);
        assert_eq!(
            response.error_kind,
            Some(ProviderHealthCheckErrorKind::RateLimited)
        );
        assert_eq!(response.http_status, Some(429));
    }
}
