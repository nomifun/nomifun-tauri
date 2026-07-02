pub mod anthropic;
pub mod anthropic_shared;
pub mod bedrock;
pub mod openai;
pub mod retry;
pub mod vertex;

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use nomi_config::config::{Config, ProviderType};
use nomi_types::llm::{LlmEvent, LlmRequest};

/// Unified interface for LLM API providers
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream(&self, request: &LlmRequest)
    -> Result<mpsc::Receiver<LlmEvent>, ProviderError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("SSE parse error: {0}")]
    Parse(String),
    #[error("Rate limited, retry after {retry_after_ms}ms: {message}")]
    RateLimited {
        retry_after_ms: u64,
        message: String,
    },
    #[error("Prompt too long: {0}")]
    PromptTooLong(String),
    #[error("Connection error: {0}")]
    Connection(String),
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } | ProviderError::Connection(_) => true,
            // Transient server-side faults (500/502/503/504) from an overloaded
            // gateway are the most common spurious failure and are safe to retry
            // on the pre-response / empty-content paths. 4xx are terminal.
            ProviderError::Api { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// Parse a `Retry-After` HTTP header into milliseconds, honouring the provider's
/// requested backoff instead of a fixed guess. Supports the delta-seconds form
/// (what LLM gateways send); returns `None` for an absent, non-numeric, or
/// HTTP-date value (caller falls back to its default). Clamped to 120s so a
/// hostile/huge value can't wedge the agent.
pub(crate) fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(secs.saturating_mul(1000).min(120_000))
}

/// Connection timeout for provider HTTP clients. Bounds the TCP/TLS connect
/// phase so an unreachable or non-responsive gateway fails fast.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle read timeout for provider HTTP clients. Applies to each read of the
/// (streaming) response, so a gateway that accepts the request but then stalls
/// — sending no further bytes — surfaces an error instead of hanging the turn
/// forever. Active streaming resets this on every chunk, so it only trips on a
/// genuine stall. The health-check probe has its own 30s wrapper; the live
/// conversation path previously had NO timeout at all, which turned an upstream
/// stall into a silent freeze (no output, no error).
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(test)]
static HTTP_CLIENT_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_http_client_build_count() {
    HTTP_CLIENT_BUILD_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn http_client_build_count() -> usize {
    HTTP_CLIENT_BUILD_COUNT.load(Ordering::SeqCst)
}

/// Shared reqwest client for all LLM providers, configured with connection and
/// idle-read timeouts. A stalled upstream produces a `reqwest` timeout error,
/// which the SSE loop converts into `LlmEvent::Error` (surfaced to the user as
/// `Nomi agent error: ...`) instead of an indefinite hang.
pub(crate) fn http_client() -> reqwest::Client {
    #[cfg(test)]
    HTTP_CLIENT_BUILD_COUNT.fetch_add(1, Ordering::SeqCst);

    let builder = reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .read_timeout(HTTP_READ_TIMEOUT);
    nomifun_net::proxy::apply_detected_proxy(builder)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub(crate) fn non_empty_rate_limit_message(body: String) -> String {
    if body.trim().is_empty() {
        "HTTP 429 Too Many Requests".to_owned()
    } else {
        body
    }
}

/// Create a provider from resolved config
pub fn create_provider(config: &Config) -> Arc<dyn LlmProvider> {
    let compat = config.compat.clone();

    match config.provider {
        ProviderType::Anthropic => Arc::new(
            anthropic::AnthropicProvider::new(&config.api_key, &config.base_url, compat)
                .with_cache(config.prompt_caching),
        ),
        ProviderType::OpenAI => Arc::new(openai::OpenAIProvider::new(
            &config.api_key,
            &config.base_url,
            compat,
        )),
        ProviderType::Bedrock => {
            let bc = config.bedrock.clone().unwrap_or_default();
            let region = bc
                .region
                .clone()
                .or_else(|| std::env::var("AWS_REGION").ok())
                .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
                .unwrap_or_else(|| "us-east-1".to_string());
            let credentials = bedrock::credentials_from_config(&bc);
            Arc::new(bedrock::BedrockProvider::new(
                &region,
                credentials,
                config.prompt_caching,
                compat,
            ))
        }
        ProviderType::Vertex => {
            let vc = config.vertex.clone().unwrap_or_default();
            let project_id = vc.project_id.clone().unwrap_or_default();
            let region = vc
                .region
                .clone()
                .unwrap_or_else(|| "us-central1".to_string());
            let auth = vertex::auth_from_config(&vc);
            Arc::new(vertex::VertexProvider::new(
                &project_id,
                &region,
                auth,
                config.prompt_caching,
                compat,
            ))
        }
    }
}

#[cfg(test)]
mod retryable_tests {
    use super::ProviderError;
    use super::parse_retry_after_ms;

    #[test]
    fn parse_retry_after_seconds_clamped() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(parse_retry_after_ms(&h), Some(30_000));

        let mut huge = HeaderMap::new();
        huge.insert(RETRY_AFTER, HeaderValue::from_static("99999"));
        assert_eq!(parse_retry_after_ms(&huge), Some(120_000)); // clamped

        // Absent / non-numeric (HTTP-date) -> None (caller uses its default).
        assert_eq!(parse_retry_after_ms(&HeaderMap::new()), None);
        let mut date = HeaderMap::new();
        date.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2025 07:28:00 GMT"));
        assert_eq!(parse_retry_after_ms(&date), None);
    }

    #[test]
    fn transient_5xx_is_retryable_but_4xx_is_not() {
        // Transient server-side faults (overloaded gateways) are the most common
        // spurious failure and are safe to retry on the pre-response / empty
        // paths; client errors (4xx) are terminal. (Phase 1)
        let api = |status| ProviderError::Api {
            status,
            message: "x".to_string(),
        };
        assert!(api(500).is_retryable());
        assert!(api(502).is_retryable());
        assert!(api(503).is_retryable());
        assert!(api(504).is_retryable());
        assert!(!api(400).is_retryable());
        assert!(!api(404).is_retryable());
        assert!(!api(429).is_retryable(), "429 is surfaced as RateLimited, not Api");

        assert!(
            ProviderError::RateLimited {
                retry_after_ms: 0,
                message: "x".to_string()
            }
            .is_retryable()
        );
        assert!(ProviderError::Connection("x".to_string()).is_retryable());
        assert!(!ProviderError::PromptTooLong("x".to_string()).is_retryable());
        assert!(!ProviderError::Parse("x".to_string()).is_retryable());
    }
}
