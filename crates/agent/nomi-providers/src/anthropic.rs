use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use nomi_config::compat::{self, ProviderCompat};
use nomi_types::llm::{LlmEvent, LlmRequest, ThinkingConfig};

use super::anthropic_shared;
use crate::{LlmProvider, ProviderError};

pub struct AnthropicProvider {
    api_keys: Vec<String>,
    current_api_key: AtomicUsize,
    base_url: String,
    cache_enabled: bool,
    compat: ProviderCompat,
    sanitize_tool_schemas: AtomicBool,
}

impl AnthropicProvider {
    pub fn new(api_key: &str, base_url: &str, compat: ProviderCompat) -> Self {
        Self {
            api_keys: crate::parse_api_keys(api_key),
            current_api_key: AtomicUsize::new(0),
            base_url: base_url.to_string(),
            cache_enabled: true,
            compat,
            sanitize_tool_schemas: AtomicBool::new(false),
        }
    }

    fn should_sanitize_tool_schemas(&self) -> bool {
        self.compat.sanitize_schema() || self.sanitize_tool_schemas.load(Ordering::Acquire)
    }

    pub fn with_cache(mut self, enabled: bool) -> Self {
        self.cache_enabled = enabled;
        self
    }

    fn build_headers(&self, api_key: &str) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        let api_key = HeaderValue::from_str(api_key)
            .map_err(|e| ProviderError::Connection(format!("Invalid x-api-key header: {}", e)))?;
        headers.insert("x-api-key", api_key);
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if self.cache_enabled {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("prompt-caching-2024-07-31"),
            );
        }
        Ok(headers)
    }

    fn build_request_body(&self, request: &LlmRequest, sanitize_tool_schemas: bool) -> Value {
        // Build system prompt with optional cache_control
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
            "model": request.model,
            "max_tokens": request.max_tokens,
            "system": system,
            "messages": anthropic_shared::build_messages(&request.messages, &self.compat),
            "stream": true
        });

        if !request.tools.is_empty() {
            let mut tools = anthropic_shared::build_tools(&request.tools);
            if sanitize_tool_schemas {
                for tool in &mut tools {
                    if let Some(schema) = tool.get("input_schema").cloned() {
                        tool["input_schema"] = compat::sanitize_json_schema(&schema);
                    }
                }
            }
            // Mark last tool with cache_control to cache the entire tools block
            if let Some(last) = tools.last_mut().filter(|_| self.cache_enabled) {
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

    async fn send_initial(
        client: &reqwest::Client,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
    ) -> Result<reqwest::Response, ProviderError> {
        crate::retry::with_initial_request_retry(|| async {
            let response = client
                .post(url)
                .headers(headers.clone())
                .json(body)
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let retry_after_ms = crate::parse_retry_after_ms(response.headers()).unwrap_or(5000);
            let body_text = response.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimited {
                    retry_after_ms,
                    message: crate::non_empty_rate_limit_message(body_text),
                });
            }
            Err(ProviderError::Api {
                status: status.as_u16(),
                message: body_text,
            })
        })
        .await
    }

    async fn send_initial_with_key_rotation(
        &self,
        client: &reqwest::Client,
        url: &str,
        body: &Value,
    ) -> Result<(reqwest::Response, HeaderMap), ProviderError> {
        let mut last_error = None;
        let key_count = self.api_keys.len();
        let start_index = self.current_api_key.load(Ordering::Acquire) % key_count.max(1);

        for offset in 0..key_count {
            let index = (start_index + offset) % key_count;
            let api_key = &self.api_keys[index];
            let headers = self.build_headers(api_key)?;
            match Self::send_initial(client, url, &headers, body).await {
                Ok(response) => {
                    self.current_api_key.store(index, Ordering::Release);
                    return Ok((response, headers));
                }
                Err(error) if crate::is_api_key_rotation_error(&error) && offset + 1 < key_count => {
                    let next_index = (index + 1) % key_count;
                    tracing::warn!(
                        target: "nomi_providers",
                        provider = "anthropic",
                        key_index = index + 1,
                        key_count = self.api_keys.len(),
                        error = %error,
                        "provider rejected API key; trying the next configured key"
                    );
                    self.current_api_key.store(next_index, Ordering::Release);
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ProviderError::Connection("No usable API key configured".to_owned())
        }))
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let client = crate::http_client();
        let sanitize_tool_schemas = self.should_sanitize_tool_schemas();
        let mut body = self.build_request_body(request, sanitize_tool_schemas);

        tracing::debug!(target: "nomi_providers", body = %serde_json::to_string_pretty(&body).unwrap_or_default(), "outgoing request");

        let (response, headers) = match self
            .send_initial_with_key_rotation(&client, &url, &body)
            .await
        {
            Ok(result) => result,
            Err(error)
                if !request.tools.is_empty()
                    && !sanitize_tool_schemas
                    && error.is_tool_schema_incompatible() =>
            {
                let ProviderError::Api { status, .. } = &error else {
                    unreachable!("schema classifier only accepts API errors");
                };
                tracing::warn!(
                    target: "nomi_providers",
                    provider = "anthropic",
                    status,
                    "provider rejected tool schemas; retrying with Bedrock-compatible schema roots"
                );
                body = self.build_request_body(request, true);
                let (response, headers) = self
                    .send_initial_with_key_rotation(&client, &url, &body)
                    .await?;
                self.sanitize_tool_schemas.store(true, Ordering::Release);
                (response, headers)
            }
            Err(error) => return Err(error),
        };

        let (tx, rx) = mpsc::channel(64);
        let client = client.clone();
        let url_clone = url.clone();

        tokio::spawn(async move {
            match anthropic_shared::process_sse_stream(response, &tx).await {
                anthropic_shared::StreamOutcome::Ok => {}
                anthropic_shared::StreamOutcome::FailedPartial(e) => {
                    let _ = tx.send(LlmEvent::Error(e.to_string())).await;
                }
                anthropic_shared::StreamOutcome::FailedEmpty(e) => {
                    if e.is_retryable() {
                        let mut backoff = std::time::Duration::from_secs(1);
                        let mut final_err = Some(e);
                        for attempt in 1..=crate::retry::MAX_STREAM_RETRIES {
                            backoff = crate::retry::backoff_sleep(attempt, backoff).await;
                            match crate::retry::send_and_check(&client, &url_clone, &headers, &body)
                                .await
                            {
                                Ok(resp) => {
                                    let outcome =
                                        anthropic_shared::process_sse_stream(resp, &tx).await;
                                    match crate::retry::evaluate_outcome(outcome, attempt) {
                                        Ok(None) => {
                                            final_err = None;
                                            break;
                                        }
                                        Ok(Some(e)) => {
                                            final_err = Some(e);
                                            break;
                                        }
                                        Err(_) => continue,
                                    }
                                }
                                Err(e) if attempt == crate::retry::MAX_STREAM_RETRIES => {
                                    final_err = Some(e);
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
