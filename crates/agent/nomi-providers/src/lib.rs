pub mod anthropic;
pub mod anthropic_shared;
pub mod bedrock;
pub mod openai;
pub mod retry;
pub mod vertex;

use std::sync::{Arc, OnceLock};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use nomi_config::config::{Config, ProviderType};
use nomi_types::llm::{LlmEvent, LlmRequest};

const MAX_DOUBLE_ENCODED_TOOL_ARGUMENT_BYTES: usize = 512 * 1024;

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

    pub(crate) fn is_tool_schema_incompatible(&self) -> bool {
        let ProviderError::Api { message, .. } = self else {
            return false;
        };
        let lower = message.to_ascii_lowercase();
        lower.contains("tool_schema_invalid")
            || (lower.contains("input_schema")
                && lower.contains("top level")
                && ["oneof", "allof", "anyof"]
                    .iter()
                    .any(|keyword| lower.contains(keyword)))
    }

    /// A number of otherwise OpenAI-compatible gateways implement streaming
    /// but reject the optional `stream_options.include_usage` extension. This
    /// is safe to retry without the extension because it only removes token
    /// accounting metadata; response content and tool semantics are unchanged.
    pub(crate) fn is_stream_usage_options_incompatible(&self) -> bool {
        let ProviderError::Api {
            status: 400 | 404 | 422,
            message,
        } = self
        else {
            return false;
        };
        let lower = message.to_ascii_lowercase();
        let names_usage_extension =
            lower.contains("stream_options") || lower.contains("include_usage");
        let rejects_parameter = [
            "unsupported",
            "not supported",
            "unknown",
            "unrecognized",
            "not permitted",
            "not allowed",
            "extra_forbidden",
            "extra inputs",
            "invalid parameter",
        ]
        .iter()
        .any(|signal| lower.contains(signal));
        names_usage_extension && rejects_parameter
    }
}

/// Split the stored provider credential into individual API keys.
///
/// Provider settings persist multiple credentials as a comma-separated string;
/// older data may use one key per line. Keep this parser in the provider layer
/// so every Nomi execution path (interactive sessions, compaction and one-shot
/// sidecars) sends one credential per HTTP request rather than the whole list
/// as a single invalid bearer token.
pub(crate) fn parse_api_keys(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn is_api_key_rotation_error(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Api {
            status: 401 | 403,
            ..
        } | ProviderError::RateLimited { .. }
    )
}

/// Parse the completed argument payload of a provider-emitted tool call.
///
/// OpenAI-compatible APIs encode function arguments as a JSON string, while
/// Anthropic-compatible streaming APIs deliver JSON fragments. In both cases
/// the completed payload must be a JSON object. A malformed payload must never
/// be replaced with `{}`: doing so turns a provider/protocol failure into a
/// seemingly valid no-argument tool call and can execute the wrong operation.
pub(crate) fn parse_tool_call_arguments(
    provider: &str,
    tool_name: &str,
    tool_id: &str,
    raw: &str,
) -> Result<Value, String> {
    if tool_name.trim().is_empty() {
        return Err(format!(
            "{provider} returned a tool call with a missing function name (call `{}`)",
            if tool_id.trim().is_empty() {
                "<missing>"
            } else {
                tool_id
            }
        ));
    }
    if tool_id.trim().is_empty() {
        return Err(format!(
            "{provider} returned tool `{tool_name}` without a call id"
        ));
    }

    let mut value = serde_json::from_str::<Value>(raw).map_err(|error| {
        format!(
            "{provider} returned malformed JSON arguments for tool `{tool_name}` (call `{tool_id}`): {error}"
        )
    })?;

    // A few OpenAI-compatible gateways double-encode the completed argument
    // object as one JSON string. Unwrap exactly one layer, with a hard size
    // bound, and still require an object. No field-level guessing or lossy
    // rewriting happens in the provider protocol parser.
    if let Value::String(encoded) = &value {
        if encoded.len() > MAX_DOUBLE_ENCODED_TOOL_ARGUMENT_BYTES {
            return Err(format!(
                "{provider} returned double-encoded arguments for tool `{tool_name}` (call `{tool_id}`) larger than the {MAX_DOUBLE_ENCODED_TOOL_ARGUMENT_BYTES}-byte safety limit"
            ));
        }
        value = serde_json::from_str::<Value>(encoded).map_err(|error| {
            format!(
                "{provider} returned malformed double-encoded JSON arguments for tool `{tool_name}` (call `{tool_id}`): {error}"
            )
        })?;
    }

    if !value.is_object() {
        return Err(format!(
            "{provider} returned non-object arguments for tool `{tool_name}` (call `{tool_id}`); expected a JSON object, got {}",
            json_value_kind(&value)
        ));
    }

    Ok(value)
}

fn json_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
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
pub(crate) fn http_client_build_count() -> usize {
    HTTP_CLIENT_BUILD_COUNT.load(Ordering::SeqCst)
}

/// Process-wide shared reqwest client for all LLM providers, configured with
/// connection and idle-read timeouts. Built exactly once (lazily, on first use)
/// so its keep-alive connection pool is reused across every request and every
/// provider. Previously a fresh client was built on every `stream()` call, which
/// gave each request an empty pool and thus a cold TCP+TLS handshake on the
/// first-token path of EVERY turn — the single largest avoidable首字 cost.
///
/// A stalled upstream produces a `reqwest` timeout error, which the SSE loop
/// converts into `LlmEvent::Error` (surfaced as `Nomi agent error: ...`) instead
/// of an indefinite hang. The detected proxy is captured at first build; a
/// runtime proxy change takes effect on the next app start.
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            #[cfg(test)]
            HTTP_CLIENT_BUILD_COUNT.fetch_add(1, Ordering::SeqCst);

            let builder = reqwest::Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .read_timeout(HTTP_READ_TIMEOUT);
            nomifun_net::proxy::apply_detected_proxy(builder)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
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
    use super::{
        is_api_key_rotation_error, parse_api_keys, parse_retry_after_ms,
        parse_tool_call_arguments, MAX_DOUBLE_ENCODED_TOOL_ARGUMENT_BYTES,
    };

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

    #[test]
    fn tool_schema_classifier_accepts_bedrock_gateway_signals() {
        let reason = ProviderError::Api {
            status: 500,
            message: r#"{"reason":"TOOL_SCHEMA_INVALID"}"#.into(),
        };
        let wording = ProviderError::Api {
            status: 400,
            message: "input_schema does not support oneOf, allOf, or anyOf at the top level".into(),
        };
        assert!(reason.is_tool_schema_incompatible());
        assert!(wording.is_tool_schema_incompatible());
    }

    #[test]
    fn tool_schema_classifier_rejects_unrelated_failures() {
        let errors = [
            ProviderError::Api {
                status: 500,
                message: "upstream unavailable".into(),
            },
            ProviderError::Api {
                status: 400,
                message: "input_schema is malformed".into(),
            },
            ProviderError::Connection("input_schema connection reset".into()),
        ];
        assert!(
            errors
                .iter()
                .all(|error| !error.is_tool_schema_incompatible())
        );
    }

    #[test]
    fn stream_usage_options_classifier_is_narrow() {
        for message in [
            "unknown parameter: stream_options",
            r#"{\"detail\":[{\"loc\":[\"body\",\"stream_options\"],\"type\":\"extra_forbidden\"}]}"#,
            "include_usage is not supported",
        ] {
            assert!(ProviderError::Api {
                status: 400,
                message: message.into(),
            }
            .is_stream_usage_options_incompatible());
        }

        for error in [
            ProviderError::Api {
                status: 500,
                message: "unknown parameter: stream_options".into(),
            },
            ProviderError::Api {
                status: 400,
                message: "streaming is unsupported".into(),
            },
            ProviderError::Connection("stream_options reset".into()),
        ] {
            assert!(!error.is_stream_usage_options_incompatible());
        }
    }

    #[test]
    fn api_key_list_supports_comma_and_legacy_newline_separators() {
        assert_eq!(
            parse_api_keys(" key-one,\nkey-two\r\n, key-three "),
            vec!["key-one", "key-two", "key-three"]
        );
        assert!(parse_api_keys(" , \n ").is_empty());
    }

    #[test]
    fn auth_and_rate_limit_errors_rotate_api_keys() {
        for status in [401, 403] {
            assert!(is_api_key_rotation_error(&ProviderError::Api {
                status,
                message: "rejected".into(),
            }));
        }
        assert!(is_api_key_rotation_error(&ProviderError::RateLimited {
            retry_after_ms: 1000,
            message: "limited".into(),
        }));
        assert!(!is_api_key_rotation_error(&ProviderError::Api {
            status: 400,
            message: "bad request".into(),
        }));
        assert!(!is_api_key_rotation_error(&ProviderError::Api {
            status: 500,
            message: "server error".into(),
        }));
    }

    #[test]
    fn tool_call_arguments_require_valid_json_object() {
        assert_eq!(
            parse_tool_call_arguments("test", "no_args", "call_ok", "{}")
                .expect("an explicit empty object is a valid no-argument call"),
            serde_json::json!({})
        );
        assert_eq!(
            parse_tool_call_arguments(
                "test",
                "update",
                "call_update",
                r#"{"kb_id":"kb_1"}"#,
            )
            .expect("object arguments should parse")["kb_id"],
            "kb_1"
        );

        let double_encoded =
            serde_json::to_string(&serde_json::json!({"path": "README.md"}).to_string()).unwrap();
        assert_eq!(
            parse_tool_call_arguments(
                "test",
                "read_file",
                "call_double",
                &double_encoded,
            )
            .expect("one whole-object JSON string layer should be unwrapped"),
            serde_json::json!({"path": "README.md"})
        );

        let triple_encoded = serde_json::to_string(&double_encoded).unwrap();
        let triple_error =
            parse_tool_call_arguments("test", "read_file", "call_triple", &triple_encoded)
                .expect_err("the compatibility parser must unwrap at most one layer");
        assert!(triple_error.contains("non-object arguments"));
        assert!(triple_error.contains("string"));

        let oversized_inner = "x".repeat(MAX_DOUBLE_ENCODED_TOOL_ARGUMENT_BYTES + 1);
        let oversized_outer = serde_json::to_string(&oversized_inner).unwrap();
        let oversized_error =
            parse_tool_call_arguments("test", "read_file", "call_large", &oversized_outer)
                .expect_err("double-encoded arguments must be bounded");
        assert!(oversized_error.contains("safety limit"));

        let malformed =
            parse_tool_call_arguments("test", "update", "call_bad", r#"{"kb_id":]"#)
                .expect_err("malformed JSON must fail instead of becoming an empty object");
        assert!(malformed.contains("malformed JSON arguments"));
        assert!(malformed.contains("call_bad"));

        let non_object = parse_tool_call_arguments("test", "update", "call_array", "[]")
            .expect_err("tool arguments must be an object");
        assert!(non_object.contains("non-object arguments"));
        assert!(non_object.contains("array"));

        let missing_name = parse_tool_call_arguments("test", " ", "call_named", "{}")
            .expect_err("a call without a function name must fail");
        assert!(missing_name.contains("missing function name"));
        assert!(missing_name.contains("call_named"));

        let missing_id = parse_tool_call_arguments("test", "update", "", "{}")
            .expect_err("a call without an id must fail");
        assert!(missing_id.contains("without a call id"));
    }
}
