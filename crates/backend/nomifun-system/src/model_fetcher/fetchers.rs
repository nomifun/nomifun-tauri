use std::time::Duration;

use axum::http::StatusCode;
use nomifun_api_types::ModelInfo;
use nomifun_common::AppError;
use serde::Deserialize;
use tracing::warn;

use super::FetchConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Dispatch to the appropriate platform-specific fetcher.
pub(crate) async fn fetch_for_platform(
    client: &reqwest::Client,
    config: &FetchConfig,
) -> Result<Vec<ModelInfo>, AppError> {
    match config.platform.as_str() {
        "anthropic" | "claude" => fetch_anthropic(client, &config.base_url, &config.api_key).await,
        "gemini" => fetch_gemini(client, &config.base_url, &config.api_key).await,
        "bedrock" => fetch_bedrock(config).await,
        "vertex-ai" => Ok(vertex_ai_models()),
        "new-api" => fetch_new_api(client, &config.base_url, &config.api_key).await,
        "mimo" | "mimo-token-plan-cn" | "mimo-token-plan-sgp" | "mimo-token-plan-ams" => {
            Ok(mimo_models())
        }
        "stepfun" => fetch_stepfun(client, &config.base_url, &config.api_key).await,
        "minimax" => Ok(minimax_models()),
        "minimax-code" | "minimax-coding-plan" => Ok(minimax_code_models()),
        "ark-coding-plan" => Ok(ark_coding_plan_models()),
        "ark-agent-plan" => fetch_ark_agent_plan(client, &config.base_url, &config.api_key).await,
        "stepfun-plan" => Ok(stepfun_plan_models()),
        "dashscope-coding" => {
            fetch_dashscope_coding(client, &config.base_url, &config.api_key).await
        }
        "glm-coding-plan" => Ok(glm_coding_plan_models()),
        "qianfan-coding-plan" => Ok(qianfan_coding_plan_models()),
        _ => fetch_openai_compatible(client, &config.base_url, &config.api_key).await,
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible (default)
// ---------------------------------------------------------------------------

/// Response shape for OpenAI `/models` endpoint.
#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
}

/// Fetch models from an OpenAI-compatible `/models` endpoint.
pub(super) async fn fetch_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| remote_error(&e))?;

    check_response_status(&resp)?;

    let body: OpenAiModelsResponse = resp
        .json()
        .await
        .map_err(|_| AppError::BadGateway("Remote models response was not valid JSON".into()))?;

    Ok(body
        .data
        .into_iter()
        .map(|m| ModelInfo {
            id: m.id,
            name: None,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Response shape for Anthropic `/v1/models`.
#[derive(Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModel>,
}

#[derive(Deserialize)]
struct AnthropicModel {
    id: String,
}

const ANTHROPIC_FALLBACK_MODELS: &[&str] = &[
    "claude-sonnet-4-20250514",
    "claude-opus-4-20250514",
    "claude-3-7-sonnet-20250219",
];

async fn fetch_anthropic(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let result = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let body: AnthropicModelsResponse = resp.json().await.map_err(|_| {
                AppError::BadGateway("Anthropic models response was not valid JSON".into())
            })?;
            Ok(body
                .data
                .into_iter()
                .map(|m| ModelInfo {
                    id: m.id,
                    name: None,
                })
                .collect())
        }
        Ok(resp) if is_catalog_availability_status(resp.status()) => {
            warn!(
                status = %resp.status(),
                "Anthropic models API failed, using fallback list"
            );
            Ok(fallback_models(ANTHROPIC_FALLBACK_MODELS))
        }
        Ok(resp) => {
            check_response_status(&resp)?;
            unreachable!("a non-success response cannot pass check_response_status")
        }
        Err(e) => {
            warn_remote_request_failure("anthropic", &e);
            Ok(fallback_models(ANTHROPIC_FALLBACK_MODELS))
        }
    }
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GeminiModelsResponse {
    models: Vec<GeminiModel>,
}

#[derive(Deserialize)]
struct GeminiModel {
    name: String,
}

const GEMINI_FALLBACK_MODELS: &[&str] = &["gemini-2.5-pro", "gemini-2.5-flash"];

async fn fetch_gemini(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    let url = format!(
        "{}/v1beta/models?key={api_key}",
        base_url.trim_end_matches('/')
    );
    let result = client.get(&url).timeout(REQUEST_TIMEOUT).send().await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            // Do not include reqwest's decode error in the response or logs:
            // its attached URL contains Gemini's `?key=...` credential.
            let body: GeminiModelsResponse = resp.json().await.map_err(|_| {
                AppError::BadGateway("Gemini models response was not valid JSON".into())
            })?;
            let models = body
                .models
                .into_iter()
                .map(|m| {
                    // Strip "models/" prefix: "models/gemini-2.5-pro" -> "gemini-2.5-pro"
                    let id = m.name.strip_prefix("models/").unwrap_or(&m.name).to_owned();
                    ModelInfo { id, name: None }
                })
                .collect();
            Ok(models)
        }
        Ok(resp) if is_catalog_availability_status(resp.status()) => {
            warn!(
                status = %resp.status(),
                "Gemini models API failed, using fallback list"
            );
            Ok(fallback_models(GEMINI_FALLBACK_MODELS))
        }
        Ok(resp) => {
            check_response_status(&resp)?;
            unreachable!("a non-success response cannot pass check_response_status")
        }
        Err(e) => {
            // `reqwest::Error` formats the full request URL. Gemini authenticates
            // in the query string, so logging `%e` would persist the API key.
            warn_remote_request_failure("gemini", &e);
            Ok(fallback_models(GEMINI_FALLBACK_MODELS))
        }
    }
}

// ---------------------------------------------------------------------------
// Bedrock (AWS SDK)
// ---------------------------------------------------------------------------

async fn fetch_bedrock(config: &FetchConfig) -> Result<Vec<ModelInfo>, AppError> {
    let bedrock_cfg = config
        .bedrock_config
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Bedrock requires bedrockConfig".into()))?;

    let region = aws_sdk_bedrock::config::Region::new(bedrock_cfg.region.clone());

    let sdk_config = match bedrock_cfg.auth_method {
        nomifun_api_types::BedrockAuthMethod::AccessKey => {
            let key_id = bedrock_cfg
                .access_key_id
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("accessKeyId is required".into()))?;
            let secret = bedrock_cfg
                .secret_access_key
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("secretAccessKey is required".into()))?;

            let creds = aws_sdk_bedrock::config::Credentials::new(
                key_id, secret, None, // session token
                None, // expiry
                "nomifun",
            );
            aws_sdk_bedrock::Config::builder()
                .region(region)
                .credentials_provider(creds)
                .build()
        }
        nomifun_api_types::BedrockAuthMethod::Profile => {
            let profile = bedrock_cfg.profile.as_deref().unwrap_or("default");
            let aws_cfg = aws_config::from_env()
                .profile_name(profile)
                .region(aws_config::Region::new(bedrock_cfg.region.clone()))
                .load()
                .await;
            aws_sdk_bedrock::Config::new(&aws_cfg)
        }
    };

    let client = aws_sdk_bedrock::Client::from_conf(sdk_config);
    let resp = client
        .list_inference_profiles()
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("Bedrock API error: {e}")))?;

    let profiles = resp.inference_profile_summaries();
    // Filter to only anthropic.claude models per API Spec
    let models: Vec<ModelInfo> = profiles
        .iter()
        .filter(|p| p.inference_profile_id().starts_with("anthropic.claude"))
        .map(|p| ModelInfo {
            id: p.inference_profile_id().to_string(),
            name: None,
        })
        .collect();

    Ok(models)
}

// ---------------------------------------------------------------------------
// Hardcoded platforms
// ---------------------------------------------------------------------------

fn vertex_ai_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "gemini-2.5-pro".into(),
            name: None,
        },
        ModelInfo {
            id: "gemini-2.5-flash".into(),
            name: None,
        },
    ]
}

fn minimax_models() -> Vec<ModelInfo> {
    let mut models = minimax_code_models();
    models.extend(fallback_models(&[
        "MiniMax-Text-01",
        "abab6.5s-chat",
        "abab6.5-chat",
    ]));
    models
}

fn mimo_models() -> Vec<ModelInfo> {
    fallback_models(&["mimo-v2.5-pro", "mimo-v2.5"])
}

fn minimax_code_models() -> Vec<ModelInfo> {
    fallback_models(&[
        "MiniMax-M3",
        "MiniMax-M2.7",
        "MiniMax-M2.7-highspeed",
        "MiniMax-M2.5",
        "MiniMax-M2.5-highspeed",
        "MiniMax-M2.1",
        "MiniMax-M2.1-highspeed",
        "MiniMax-M2",
    ])
}

fn ark_coding_plan_models() -> Vec<ModelInfo> {
    fallback_models(&["ark-code-latest"])
}

// ---------------------------------------------------------------------------
// Ark Agent Plan (remote catalog with fallback)
// ---------------------------------------------------------------------------

/// Switchable model set exposed by the Agent Plan router, used when the plan
/// gateway does not serve a `/models` catalog (the `/api/plan/v3` endpoint
/// only routes `/chat/completions` — `/models` returns 404). `ark-code-latest`
/// is the console-switchable router alias (recommended). The rest are the
/// concrete IDs verified to be accepted by the Agent Plan endpoint; other Ark
/// model IDs return `UnsupportedModel` there. Users can still type any ID.
const ARK_AGENT_PLAN_FALLBACK_MODELS: &[&str] = &[
    "ark-code-latest",
    "doubao-seed-2.0-code",
    "doubao-seed-2.0-pro",
    "doubao-seed-2.0-lite",
    "deepseek-v4-flash",
    "glm-5.2",
    "kimi-k2.6",
    "minimax-m2.7",
];

/// Ark Agent Plan: pull the model list from the official OpenAI-compatible
/// `/models` endpoint on the coding/agent base URL. The subscription gateway
/// often only routes `/chat/completions` (per Volcengine's "plan keys are for
/// coding/agent tools, not arbitrary API calls" policy), so on availability
/// failures or an empty catalog we fall back to the known switchable set.
/// Authentication and request errors are still returned to the caller.
/// Mirrors the fetch-then-fallback pattern used by `fetch_anthropic` /
/// `fetch_gemini`.
async fn fetch_ark_agent_plan(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    match fetch_openai_compatible(client, base_url, api_key).await {
        Ok(models) if !models.is_empty() => Ok(models),
        Ok(_) => {
            warn!("Ark Agent Plan models API returned empty list, using fallback");
            Ok(fallback_models(ARK_AGENT_PLAN_FALLBACK_MODELS))
        }
        Err(e)
            if is_catalog_availability_error(&e)
                || matches!(&e, AppError::BadRequest(_)) =>
        {
            warn!(error = %e, "Ark Agent Plan models API unavailable, using fallback list");
            Ok(fallback_models(ARK_AGENT_PLAN_FALLBACK_MODELS))
        }
        Err(e) => Err(e),
    }
}

fn stepfun_plan_models() -> Vec<ModelInfo> {
    fallback_models(&[
        "step-3.7-flash",
        "step-router-v1",
        "step-3.5-flash-2603",
        "step-3.5-flash",
    ])
}

// ---------------------------------------------------------------------------
// StepFun (remote catalog with an official-host fallback)
// ---------------------------------------------------------------------------

/// Stable public StepFun chat model IDs documented by the provider. The live
/// `/v1/models` catalog remains authoritative; this list is only used when the
/// official host is temporarily unreachable, rate-limited, returns 5xx, sends
/// malformed JSON, or returns an empty catalog.
///
/// Keep plan-only `step-router-v1` out of this list: it is not callable through
/// the regular `https://api.stepfun.com/v1` billing endpoint.
const STEPFUN_FALLBACK_MODELS: &[&str] = &[
    "step-3.5-flash-2603",
    "step-3.5-flash",
    "step-3",
    "step-2-mini",
    "step-2-16k",
    "step-1o-turbo-vision",
    "step-1o-vision-32k",
    "step-1v-32k",
    "step-1v-8k",
    "step-1-32k",
    "step-1-8k",
];

async fn fetch_stepfun(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    match fetch_openai_compatible(client, base_url, api_key).await {
        Ok(models) if !models.is_empty() => Ok(models),
        Ok(_) if is_official_stepfun_base_url(base_url) => {
            warn!("StepFun models API returned an empty catalog, using fallback list");
            Ok(fallback_models(STEPFUN_FALLBACK_MODELS))
        }
        Ok(models) => Ok(models),
        Err(error)
            if is_official_stepfun_base_url(base_url)
                && is_catalog_availability_error(&error) =>
        {
            warn!(
                error_code = error.error_code(),
                "StepFun models API unavailable, using fallback list"
            );
            Ok(fallback_models(STEPFUN_FALLBACK_MODELS))
        }
        Err(error) => Err(error),
    }
}

fn is_official_stepfun_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("api.stepfun.com")
        && url.port_or_known_default() == Some(443)
        && url.path().trim_end_matches('/') == "/v1"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn is_catalog_availability_error(error: &AppError) -> bool {
    matches!(
        error,
        AppError::BadGateway(_) | AppError::Timeout(_) | AppError::RateLimited
    )
}

fn is_catalog_availability_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

fn glm_coding_plan_models() -> Vec<ModelInfo> {
    fallback_models(&["glm-5.2", "glm-5", "glm-4.7"])
}

fn qianfan_coding_plan_models() -> Vec<ModelInfo> {
    fallback_models(&[
        "qianfan-code-latest",
        "qwen3.7-plus",
        "qwen3.6-plus",
        "qwen3.5-plus",
        "qwen3-max-2026-01-23",
        "qwen3-coder-next",
        "qwen3-coder-plus",
        "kimi-k2.5",
        "deepseek-v3.2",
        "glm-5",
        "minimax-m2.5",
        "MiniMax-M2.5",
        "ernie-4.5-turbo-20260402",
        "deepseek-v4-flash",
        "glm-5.1",
    ])
}

// ---------------------------------------------------------------------------
// new-api (OpenAI-compatible with /v1 enforcement)
// ---------------------------------------------------------------------------

async fn fetch_new_api(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    let normalized = ensure_v1_path(base_url);
    fetch_openai_compatible(client, &normalized, api_key).await
}

/// Ensure the URL path ends with `/v1`.
fn ensure_v1_path(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

// ---------------------------------------------------------------------------
// dashscope-coding (hardcoded + key validation)
// ---------------------------------------------------------------------------

const DASHSCOPE_MODELS: &[&str] = &["qwen-coder-plus", "qwen-coder-turbo"];

async fn fetch_dashscope_coding(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AppError> {
    // Validate key by sending a minimal chat completion request
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": DASHSCOPE_MODELS[0],
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| remote_error(&e))?;

    check_response_status(&resp)?;

    Ok(fallback_models(DASHSCOPE_MODELS))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn fallback_models(ids: &[&str]) -> Vec<ModelInfo> {
    ids.iter()
        .map(|id| ModelInfo {
            id: (*id).to_string(),
            name: None,
        })
        .collect()
}

fn check_response_status(resp: &reqwest::Response) -> Result<(), AppError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    match status {
        StatusCode::UNAUTHORIZED => {
            Err(AppError::Unauthorized("Remote API rejected the API key".into()))
        }
        StatusCode::FORBIDDEN => Err(AppError::Forbidden(
            "Remote API denied access for this API key".into(),
        )),
        StatusCode::TOO_MANY_REQUESTS => Err(AppError::RateLimited),
        status if status.is_client_error() => Err(AppError::BadRequest(format!(
            "Remote API rejected the model-list request ({status})"
        ))),
        status => Err(AppError::BadGateway(format!(
            "Remote API returned {status}"
        ))),
    }
}

fn remote_error(e: &reqwest::Error) -> AppError {
    if e.is_timeout() {
        AppError::Timeout(
            "Remote API request timed out; check the network and system proxy".into(),
        )
    } else if e.is_connect() {
        AppError::BadGateway(
            "Could not connect to the remote API; check DNS, TLS, firewall, and system proxy settings"
                .into(),
        )
    } else {
        // Never expose reqwest's Display text here. It includes the request URL,
        // which can carry credentials (notably Gemini's `?key=...`).
        AppError::BadGateway("Remote API request failed before a response was received".into())
    }
}

fn warn_remote_request_failure(provider: &str, error: &reqwest::Error) {
    warn!(
        provider,
        timeout = error.is_timeout(),
        connect = error.is_connect(),
        request = error.is_request(),
        body = error.is_body(),
        decode = error.is_decode(),
        "Provider models API unreachable, using fallback list"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_v1_path_already_present() {
        assert_eq!(
            ensure_v1_path("https://api.example.com/v1"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn ensure_v1_path_missing() {
        assert_eq!(
            ensure_v1_path("https://api.example.com"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn ensure_v1_path_trailing_slash() {
        assert_eq!(
            ensure_v1_path("https://api.example.com/"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn ensure_v1_path_with_v1_and_trailing_slash() {
        assert_eq!(
            ensure_v1_path("https://api.example.com/v1/"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn vertex_ai_returns_expected_models() {
        let models = vertex_ai_models();
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0],
            ModelInfo {
                id: "gemini-2.5-pro".into(),
                name: None
            }
        );
        assert_eq!(
            models[1],
            ModelInfo {
                id: "gemini-2.5-flash".into(),
                name: None
            }
        );
    }

    #[test]
    fn minimax_returns_expected_models() {
        let models = minimax_models();
        assert!(models.contains(&ModelInfo { id: "MiniMax-M3".into(), name: None }));
        assert!(models.contains(&ModelInfo { id: "MiniMax-M2.7".into(), name: None }));
        assert!(models.contains(&ModelInfo { id: "MiniMax-M2.5".into(), name: None }));
        assert!(models.contains(&ModelInfo { id: "MiniMax-Text-01".into(), name: None }));
    }

    #[test]
    fn mimo_models_include_current_chat_and_agent_models() {
        let models = mimo_models();
        assert!(models.contains(&ModelInfo { id: "mimo-v2.5-pro".into(), name: None }));
        assert!(models.contains(&ModelInfo { id: "mimo-v2.5".into(), name: None }));
    }

    #[test]
    fn minimax_code_plan_models_include_current_coding_models() {
        assert!(minimax_code_models().contains(&ModelInfo { id: "MiniMax-M3".into(), name: None }));
        assert!(minimax_code_models().contains(&ModelInfo {
            id: "MiniMax-M2.7-highspeed".into(),
            name: None
        }));
        assert!(minimax_code_models().contains(&ModelInfo { id: "MiniMax-M2.1".into(), name: None }));
    }

    #[test]
    fn coding_plan_fallbacks_include_default_router_models() {
        assert!(ark_coding_plan_models().contains(&ModelInfo { id: "ark-code-latest".into(), name: None }));
        assert!(stepfun_plan_models().contains(&ModelInfo { id: "step-router-v1".into(), name: None }));
        assert!(glm_coding_plan_models().contains(&ModelInfo { id: "glm-5.2".into(), name: None }));
        assert!(
            qianfan_coding_plan_models().contains(&ModelInfo {
                id: "qianfan-code-latest".into(),
                name: None
            })
        );
    }

    #[test]
    fn stepfun_fallback_has_public_models_but_not_plan_only_router() {
        let models = fallback_models(STEPFUN_FALLBACK_MODELS);
        assert!(models.contains(&ModelInfo {
            id: "step-3.5-flash-2603".into(),
            name: None
        }));
        assert!(models.contains(&ModelInfo {
            id: "step-1o-turbo-vision".into(),
            name: None
        }));
        assert!(!models.iter().any(|model| model.id == "step-router-v1"));
    }

    #[test]
    fn stepfun_fallback_is_restricted_to_exact_official_https_host() {
        assert!(is_official_stepfun_base_url(
            "https://api.stepfun.com/v1"
        ));
        assert!(is_official_stepfun_base_url(
            " https://api.stepfun.com/v1/ "
        ));
        assert!(!is_official_stepfun_base_url(
            "http://api.stepfun.com/v1"
        ));
        assert!(!is_official_stepfun_base_url(
            "https://api.stepfun.com.evil.example/v1"
        ));
        assert!(!is_official_stepfun_base_url(
            "https://proxy.example.com/v1"
        ));
        assert!(!is_official_stepfun_base_url(
            "https://api.stepfun.com/not-v1"
        ));
        assert!(!is_official_stepfun_base_url(
            "https://api.stepfun.com/v1?route=other"
        ));
    }

    #[test]
    fn catalog_fallback_never_masks_bad_credentials_or_requests() {
        assert!(is_catalog_availability_error(&AppError::BadGateway(
            "upstream 500".into()
        )));
        assert!(is_catalog_availability_error(&AppError::Timeout(
            "slow".into()
        )));
        assert!(is_catalog_availability_error(&AppError::RateLimited));
        assert!(!is_catalog_availability_error(&AppError::Unauthorized(
            "bad key".into()
        )));
        assert!(!is_catalog_availability_error(&AppError::Forbidden(
            "no access".into()
        )));
        assert!(!is_catalog_availability_error(&AppError::BadRequest(
            "bad endpoint".into()
        )));
    }

    #[tokio::test]
    async fn remote_transport_error_does_not_expose_url_credentials() {
        let secret = "must-not-appear";
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:1/models?key={secret}"))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .unwrap_err();

        let public_error = remote_error(&error).to_string();
        assert!(!public_error.contains(secret));
        assert!(!public_error.contains("?key="));
        assert!(public_error.contains("Could not connect"));
    }

    #[test]
    fn ark_agent_plan_fallback_includes_router_alias_and_families() {
        let models = fallback_models(ARK_AGENT_PLAN_FALLBACK_MODELS);
        // Router alias must be present — it is the recommended, console-switchable entry.
        assert!(models.contains(&ModelInfo { id: "ark-code-latest".into(), name: None }));
        // A couple of the concrete IDs verified against the live Agent Plan endpoint.
        assert!(models.contains(&ModelInfo { id: "glm-5.2".into(), name: None }));
        assert!(models.contains(&ModelInfo {
            id: "deepseek-v4-flash".into(),
            name: None
        }));
    }

    #[test]
    fn fallback_models_builds_model_info_list() {
        let models = fallback_models(&["a", "b", "c"]);
        assert_eq!(models.len(), 3);
        assert_eq!(
            models[0],
            ModelInfo {
                id: "a".into(),
                name: None
            }
        );
    }
}
