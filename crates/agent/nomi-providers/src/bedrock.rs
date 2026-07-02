// AWS Bedrock provider for Claude models.
// Uses AWS SigV4 authentication and AWS event stream binary framing.

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    self as sigv4_http, PayloadChecksumKind, SignableBody, SignableRequest, SignatureLocation,
    SigningSettings,
};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::time::SystemTime;
use tokio::sync::mpsc;

use base64::Engine as _;

use nomi_types::llm::{LlmEvent, LlmRequest, ThinkingConfig};
use nomi_types::message::{StopReason, TokenUsage};

use super::anthropic_shared;
use crate::{LlmProvider, ProviderError};
use nomi_config::compat::{self, ProviderCompat};

pub struct BedrockProvider {
    region: String,
    credentials: AwsCredentials,
    cache_enabled: bool,
    compat: ProviderCompat,
}

#[derive(Debug, Clone)]
pub enum AwsCredentials {
    Explicit {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    Profile(String),
    Environment,
}

impl BedrockProvider {
    pub fn new(
        region: &str,
        credentials: AwsCredentials,
        cache_enabled: bool,
        compat: ProviderCompat,
    ) -> Self {
        Self {
            region: region.to_string(),
            credentials,
            cache_enabled,
            compat,
        }
    }

    fn build_request_body(&self, request: &LlmRequest) -> Value {
        let system = if self.cache_enabled {
            json!([{
                "type": "text",
                "text": &request.system,
                "cache_control": { "type": "ephemeral" }
            }])
        } else {
            json!(&request.system)
        };

        let mut body = json!({
            "anthropic_version": "bedrock-2023-05-31",
            "max_tokens": request.max_tokens,
            "system": system,
            "messages": anthropic_shared::build_messages(&request.messages, &self.compat)
        });

        if !request.tools.is_empty() {
            let mut tools = anthropic_shared::build_tools(&request.tools);
            if self.compat.sanitize_schema() {
                for tool in &mut tools {
                    if let Some(schema) = tool.get("input_schema").cloned() {
                        tool["input_schema"] = compat::sanitize_json_schema(&schema);
                    }
                }
            }
            if self.cache_enabled
                && let Some(last) = tools.last_mut()
            {
                last["cache_control"] = json!({ "type": "ephemeral" });
            }
            body["tools"] = json!(tools);
        }

        if let Some(ThinkingConfig::Enabled { budget_tokens }) = &request.thinking {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget_tokens
            });
        }

        body
    }

    fn build_url(&self, model: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/invoke-with-response-stream",
            self.region, model
        )
    }

    fn resolve_credentials(&self) -> Result<Credentials, ProviderError> {
        match &self.credentials {
            AwsCredentials::Explicit {
                access_key_id,
                secret_access_key,
                session_token,
            } => Ok(Credentials::new(
                access_key_id,
                secret_access_key,
                session_token.clone(),
                None,
                "nomi",
            )),
            AwsCredentials::Profile(profile) => Self::credentials_from_sdk(Some(profile.clone())),
            AwsCredentials::Environment => Self::credentials_from_sdk(None),
        }
    }

    fn credentials_from_sdk(profile: Option<String>) -> Result<Credentials, ProviderError> {
        // Use a short-lived tokio runtime to resolve credentials synchronously.
        // This is called once per LLM request so the overhead is acceptable.
        let rt = tokio::runtime::Handle::try_current();

        let resolve = async move {
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
            if let Some(p) = profile {
                loader = loader.profile_name(p);
            }
            let config = loader.load().await;
            let provider = config.credentials_provider().ok_or_else(|| {
                ProviderError::Connection(
                    "No AWS credentials found. Set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, \
                     AWS_PROFILE, or configure credentials in ~/.aws/credentials"
                        .into(),
                )
            })?;

            use aws_credential_types::provider::ProvideCredentials;
            let creds = provider
                .provide_credentials()
                .await
                .map_err(|e| ProviderError::Connection(format!("AWS credential error: {}", e)))?;

            Ok(Credentials::new(
                creds.access_key_id(),
                creds.secret_access_key(),
                creds.session_token().map(|s| s.to_string()),
                creds.expiry(),
                "nomi-sdk",
            ))
        };

        match rt {
            Ok(_handle) => {
                // Already inside a tokio runtime — use spawn_blocking to avoid nested block_on
                std::thread::scope(|s| {
                    s.spawn(|| {
                        tokio::runtime::Runtime::new()
                            .map_err(|e| {
                                ProviderError::Connection(format!("Runtime error: {}", e))
                            })?
                            .block_on(resolve)
                    })
                    .join()
                    .unwrap()
                })
            }
            Err(_) => {
                // No runtime — safe to create one
                tokio::runtime::Runtime::new()
                    .map_err(|e| ProviderError::Connection(format!("Runtime error: {}", e)))?
                    .block_on(resolve)
            }
        }
    }

    fn sign_bedrock_request(
        region: &str,
        method: &str,
        url: &str,
        headers: &HeaderMap,
        body: &[u8],
        credentials: &Credentials,
    ) -> Result<HeaderMap, ProviderError> {
        let mut signing_settings = SigningSettings::default();
        signing_settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        signing_settings.signature_location = SignatureLocation::Headers;

        let identity = credentials.clone().into();
        let signing_params = aws_sigv4::sign::v4::SigningParams::builder()
            .identity(&identity)
            .region(region)
            .name("bedrock")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()
            .map_err(|e| ProviderError::Connection(format!("SigV4 params error: {}", e)))?;

        // Build header pairs for signing
        let header_pairs: Vec<(&str, &str)> = headers
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str(), v)))
            .collect();

        let signable_request = SignableRequest::new(
            method,
            url,
            header_pairs.into_iter(),
            SignableBody::Bytes(body),
        )
        .map_err(|e| ProviderError::Connection(format!("Signable request error: {}", e)))?;

        let (signing_instructions, _signature) =
            sigv4_http::sign(signable_request, &signing_params.into())
                .map_err(|e| ProviderError::Connection(format!("SigV4 signing error: {}", e)))?
                .into_parts();

        let mut signed_headers = headers.clone();
        for (name, value) in signing_instructions.headers() {
            signed_headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| ProviderError::Connection(format!("Header name error: {}", e)))?,
                HeaderValue::from_str(value)
                    .map_err(|e| ProviderError::Connection(format!("Header value error: {}", e)))?,
            );
        }

        Ok(signed_headers)
    }
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let url = self.build_url(&request.model);
        let body = self.build_request_body(request);

        tracing::debug!(target: "nomi_providers", body = %serde_json::to_string_pretty(&body).unwrap_or_default(), "outgoing request");

        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| ProviderError::Connection(format!("JSON serialize error: {}", e)))?;

        let credentials = self.resolve_credentials()?;
        let client = crate::http_client();

        // Each attempt re-signs: SigV4 signatures embed a timestamp and are only
        // valid within a short window, so a retried request needs a fresh
        // signature. `send_signed` builds headers, signs, sends, and maps a
        // non-2xx status — used for both the initial connect-retry and the
        // mid-stream retry loop (parity with the other providers, which retry via
        // crate::retry). (Phase 1 provider retry parity)
        let send_signed = |region: &str,
                           client: &reqwest::Client,
                           url: &str,
                           body_bytes: &[u8],
                           credentials: &Credentials| {
            let region = region.to_owned();
            let client = client.clone();
            let url = url.to_owned();
            let body_bytes = body_bytes.to_vec();
            let credentials = credentials.clone();
            async move {
                let mut headers = HeaderMap::new();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                let signed = Self::sign_bedrock_request(
                    &region, "POST", &url, &headers, &body_bytes, &credentials,
                )?;
                let response = client
                    .post(&url)
                    .headers(signed)
                    .body(body_bytes.clone())
                    .send()
                    .await
                    .map_err(|e| ProviderError::Connection(e.to_string()))?;
                let status = response.status();
                if !status.is_success() {
                    let retry_after_ms =
                        crate::parse_retry_after_ms(response.headers()).unwrap_or(5000);
                    let body_text = response.text().await.unwrap_or_default();
                    if status.as_u16() == 429 {
                        return Err(ProviderError::RateLimited {
                            retry_after_ms,
                            message: crate::non_empty_rate_limit_message(body_text),
                        });
                    }
                    return Err(ProviderError::Api {
                        status: status.as_u16(),
                        message: format_bedrock_error(status.as_u16(), &body_text),
                    });
                }
                Ok(response)
            }
        };

        // Initial request with connect-failure retry (status/rate-limit errors
        // are surfaced immediately, same as the other providers).
        let response = crate::retry::with_initial_connect_retry(|| {
            send_signed(&self.region, &client, &url, &body_bytes, &credentials)
        })
        .await?;

        let (tx, rx) = mpsc::channel(64);

        // Owned copies for the spawned task's mid-stream retry loop (it outlives
        // `&self`, so it cannot borrow region/client/credentials).
        let region = self.region.clone();
        let client = client.clone();
        let url_owned = url.clone();

        // AWS event stream uses binary framing.
        tokio::spawn(async move {
            match process_aws_event_stream(response, &tx).await {
                anthropic_shared::StreamOutcome::Ok => {}
                anthropic_shared::StreamOutcome::FailedPartial(e) => {
                    // Content already emitted — replaying would duplicate it.
                    let _ = tx.send(LlmEvent::Error(e.to_string())).await;
                }
                anthropic_shared::StreamOutcome::FailedEmpty(e) => {
                    if e.is_retryable() {
                        let mut backoff = std::time::Duration::from_secs(1);
                        let mut final_err = Some(e);
                        for attempt in 1..=crate::retry::MAX_STREAM_RETRIES {
                            backoff = crate::retry::backoff_sleep(attempt, backoff).await;
                            match send_signed(&region, &client, &url_owned, &body_bytes, &credentials)
                                .await
                            {
                                Ok(resp) => {
                                    let outcome = process_aws_event_stream(resp, &tx).await;
                                    match crate::retry::evaluate_outcome(outcome, attempt) {
                                        Ok(None) => {
                                            final_err = None;
                                            break;
                                        }
                                        Ok(Some(err)) => {
                                            final_err = Some(err);
                                            break;
                                        }
                                        Err(_) => continue,
                                    }
                                }
                                Err(err) if attempt == crate::retry::MAX_STREAM_RETRIES => {
                                    final_err = Some(err);
                                    break;
                                }
                                Err(_) => continue,
                            }
                        }
                        if let Some(err) = final_err {
                            let _ = tx.send(LlmEvent::Error(err.to_string())).await;
                        }
                    } else {
                        let _ = tx.send(LlmEvent::Error(e.to_string())).await;
                    }
                }
            }
        });

        Ok(rx)
    }
}

/// Process the AWS event stream (binary framed) from Bedrock
async fn process_aws_event_stream(
    response: reqwest::Response,
    tx: &mpsc::Sender<LlmEvent>,
) -> anthropic_shared::StreamOutcome {
    use futures::StreamExt;

    let mut state = anthropic_shared::StreamState::new();
    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    let mut emitted_content = false;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let err = ProviderError::Connection(e.to_string());
                return if emitted_content {
                    anthropic_shared::StreamOutcome::FailedPartial(err)
                } else {
                    anthropic_shared::StreamOutcome::FailedEmpty(err)
                };
            }
        };
        buffer.extend_from_slice(&chunk);

        // Parse complete AWS event stream messages from buffer
        while let Some((event_data, consumed)) = parse_aws_event(&buffer) {
            buffer = buffer[consumed..].to_vec();

            if let Some(payload) = event_data {
                // The payload contains an SSE-like structure with "bytes" field
                if let Ok(wrapper) = serde_json::from_slice::<Value>(&payload) {
                    // Bedrock wraps the payload in {"bytes": "base64-encoded-data"}
                    if let Some(b64) = wrapper["bytes"].as_str()
                        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64)
                        && let Ok(inner) = String::from_utf8(decoded)
                    {
                        tracing::debug!(target: "nomi_providers", chunk = %inner, "bedrock event chunk");
                        // Inner payload is JSON with event type hints
                        if let Ok(json_val) = serde_json::from_str::<Value>(&inner) {
                            let event_type = json_val["type"].as_str().unwrap_or("");
                            let events =
                                anthropic_shared::parse_sse_data(event_type, &inner, &mut state);
                            for event in events {
                                if matches!(
                                    event,
                                    LlmEvent::TextDelta(_)
                                        | LlmEvent::ThinkingDelta(_)
                                        | LlmEvent::ThinkingSignature(_)
                                        | LlmEvent::ToolUse { .. }
                                ) {
                                    emitted_content = true;
                                }
                                if tx.send(event).await.is_err() {
                                    return anthropic_shared::StreamOutcome::Ok;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // If we haven't sent a Done event, send one now
    if state.input_tokens > 0 || state.output_tokens > 0 {
        let _ = tx
            .send(LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: state.input_tokens,
                    output_tokens: state.output_tokens,
                    cache_creation_tokens: state.cache_creation_tokens,
                    cache_read_tokens: state.cache_read_tokens,
                },
            })
            .await;
    }

    anthropic_shared::StreamOutcome::Ok
}

/// Parse one AWS event stream message from the buffer.
/// Returns (Some(payload), bytes_consumed) if a complete message is found,
/// or None if more data is needed.
///
/// AWS event stream binary format:
/// - Prelude: total_len (4 bytes, big-endian) + headers_len (4 bytes) + prelude_crc (4 bytes)
/// - Headers: variable length
/// - Payload: variable length
/// - Message CRC: 4 bytes
fn parse_aws_event(buffer: &[u8]) -> Option<(Option<Vec<u8>>, usize)> {
    if buffer.len() < 12 {
        return None; // Need at least the prelude
    }

    let total_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    let headers_len = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;

    if buffer.len() < total_len {
        return None; // Incomplete message
    }

    // Prelude is 12 bytes (total_len + headers_len + prelude_crc)
    // Payload starts after prelude + headers
    let payload_start = 12 + headers_len;
    // Payload ends 4 bytes before total_len (message CRC)
    let payload_end = total_len - 4;

    if payload_start <= payload_end {
        let payload = buffer[payload_start..payload_end].to_vec();
        Some((Some(payload), total_len))
    } else {
        // Empty payload (e.g., initial response event)
        Some((None, total_len))
    }
}

/// Format Bedrock error responses with actionable hints
fn format_bedrock_error(status: u16, body: &str) -> String {
    // Try to extract the AWS error type from the response
    let error_type = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.get("__type")
            .or_else(|| v.get("type"))
            .and_then(|t| t.as_str().map(String::from))
    });

    let hint = match status {
        403 => Some(
            "Check IAM permissions: the role/user needs bedrock:InvokeModelWithResponseStream. \
             Also verify the model is enabled in the Bedrock console for your account.",
        ),
        404 => Some(
            "Model not found in this region. Verify the model ID and that it's available in \
             your configured AWS region.",
        ),
        400 => {
            if body.contains("schema") || body.contains("Schema") {
                Some(
                    "Request schema validation failed. If using tools, try enabling sanitize_schema=true in [providers.bedrock.compat].",
                )
            } else {
                Some("Bad request — check model parameters and message format.")
            }
        }
        503 | 529 => Some(
            "Service overloaded or throttled. You may have exceeded your provisioned throughput quota. \
             Retry after a moment or request a quota increase.",
        ),
        _ => None,
    };

    let type_info = error_type.map(|t| format!(" [{}]", t)).unwrap_or_default();

    match hint {
        Some(h) => format!("{}{}\nHint: {}", body, type_info, h),
        None => format!("{}{}", body, type_info),
    }
}

/// Build AwsCredentials from nomi-config's BedrockConfig
pub fn credentials_from_config(bc: &nomi_config::config::BedrockConfig) -> AwsCredentials {
    if let (Some(key_id), Some(secret)) = (&bc.access_key_id, &bc.secret_access_key) {
        AwsCredentials::Explicit {
            access_key_id: key_id.clone(),
            secret_access_key: secret.clone(),
            session_token: bc.session_token.clone(),
        }
    } else if let Some(profile) = &bc.profile {
        AwsCredentials::Profile(profile.clone())
    } else {
        AwsCredentials::Environment
    }
}
