// Google Vertex AI provider for Claude models.
// Uses GCP OAuth2 authentication. Response is standard SSE (same as Anthropic).

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use nomi_types::llm::{LlmEvent, LlmRequest, ThinkingConfig};

use super::anthropic_shared;
use crate::{LlmProvider, ProviderError};
use nomi_config::compat::ProviderCompat;

pub struct VertexProvider {
    project_id: String,
    region: String,
    auth: GcpAuth,
    cache_enabled: bool,
    compat: ProviderCompat,
    /// Cached access token
    cached_token: Mutex<Option<CachedToken>>,
}

#[derive(Debug, Clone)]
pub enum GcpAuth {
    ServiceAccount { key_file: String },
    ApplicationDefault,
    MetadataServer,
}

struct CachedToken {
    token: String,
    expires_at: u64,
}

impl VertexProvider {
    pub fn new(
        project_id: &str,
        region: &str,
        auth: GcpAuth,
        cache_enabled: bool,
        compat: ProviderCompat,
    ) -> Self {
        Self {
            project_id: project_id.to_string(),
            region: region.to_string(),
            auth,
            cache_enabled,
            compat,
            cached_token: Mutex::new(None),
        }
    }

    fn build_url(&self, model: &str) -> String {
        format!(
            "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/anthropic/models/{}:streamRawPredict",
            self.region, self.project_id, self.region, model
        )
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
            "anthropic_version": "vertex-2023-10-16",
            "max_tokens": request.max_tokens,
            "system": system,
            "messages": anthropic_shared::build_messages(&request.messages, &self.compat),
            "stream": true
        });

        if !request.tools.is_empty() {
            let mut tools = anthropic_shared::build_tools(&request.tools);
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

    async fn get_access_token(&self) -> Result<String, ProviderError> {
        // Check cache first
        {
            let cached = self.cached_token.lock().map_err(|_| {
                ProviderError::Connection("Vertex token cache lock poisoned".to_string())
            })?;
            if let Some(token) = cached.as_ref() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if token.expires_at > now + 60 {
                    return Ok(token.token.clone());
                }
            }
        }

        let (token, expires_in) = match &self.auth {
            GcpAuth::ServiceAccount { key_file } => {
                self.get_service_account_token(key_file).await?
            }
            GcpAuth::ApplicationDefault => self.get_adc_token().await?,
            GcpAuth::MetadataServer => self.get_metadata_token().await?,
        };

        // Cache the token
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut cached = self.cached_token.lock().map_err(|_| {
            ProviderError::Connection("Vertex token cache lock poisoned".to_string())
        })?;
        *cached = Some(CachedToken {
            token: token.clone(),
            expires_at: now + expires_in,
        });

        Ok(token)
    }

    async fn get_service_account_token(
        &self,
        key_file: &str,
    ) -> Result<(String, u64), ProviderError> {
        let key_json = std::fs::read_to_string(key_file)
            .map_err(|e| ProviderError::Connection(format!("Failed to read key file: {}", e)))?;

        let sa: ServiceAccountKey = serde_json::from_str(&key_json)
            .map_err(|e| ProviderError::Connection(format!("Failed to parse key file: {}", e)))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims = JwtClaims {
            iss: sa.client_email.clone(),
            scope: "https://www.googleapis.com/auth/cloud-platform".to_string(),
            aud: sa.token_uri.clone(),
            iat: now,
            exp: now + 3600,
        };

        let encoding_key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
            .map_err(|e| ProviderError::Connection(format!("Invalid RSA key: {}", e)))?;

        let header = Header::new(Algorithm::RS256);
        let jwt = jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| ProviderError::Connection(format!("JWT encode error: {}", e)))?;

        // Exchange JWT for access token
        let client = crate::http_client();
        let resp = client
            .post(&sa.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &jwt),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Connection(format!("Token exchange error: {}", e)))?;

        let token_resp: GoogleTokenResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Connection(format!("Token parse error: {}", e)))?;

        Ok((token_resp.access_token, token_resp.expires_in))
    }

    async fn get_adc_token(&self) -> Result<(String, u64), ProviderError> {
        // Read Application Default Credentials
        let adc_path = dirs::home_dir()
            .ok_or_else(|| ProviderError::Connection("Cannot determine home dir".into()))?
            .join(".config/gcloud/application_default_credentials.json");

        let adc_json = std::fs::read_to_string(&adc_path).map_err(|e| {
            ProviderError::Connection(format!(
                "Failed to read ADC at {}: {}. Run 'gcloud auth application-default login'.",
                adc_path.display(),
                e
            ))
        })?;

        let adc: AdcCredentials = serde_json::from_str(&adc_json)
            .map_err(|e| ProviderError::Connection(format!("Failed to parse ADC: {}", e)))?;

        // Use refresh token to get access token
        let client = crate::http_client();
        let resp = client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", adc.client_id.as_str()),
                ("client_secret", adc.client_secret.as_str()),
                ("refresh_token", adc.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Connection(format!("ADC token refresh error: {}", e)))?;

        let token_resp: GoogleTokenResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Connection(format!("Token parse error: {}", e)))?;

        Ok((token_resp.access_token, token_resp.expires_in))
    }

    async fn get_metadata_token(&self) -> Result<(String, u64), ProviderError> {
        let client = crate::http_client();
        let resp = client
            .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| ProviderError::Connection(format!("Metadata server error: {}", e)))?;

        let token_resp: GoogleTokenResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::Connection(format!("Token parse error: {}", e)))?;

        Ok((token_resp.access_token, token_resp.expires_in))
    }
}

#[async_trait]
impl LlmProvider for VertexProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let url = self.build_url(&request.model);
        let body = self.build_request_body(request);

        tracing::debug!(target: "nomi_providers", body = %serde_json::to_string_pretty(&body).unwrap_or_default(), "outgoing request");

        let access_token = self.get_access_token().await?;
        let client = crate::http_client();

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", access_token))
                .map_err(|e| ProviderError::Connection(format!("Header error: {}", e)))?,
        );

        let response = crate::retry::with_initial_connect_retry(|| async {
            let response = client
                .post(&url)
                .headers(headers.clone())
                .json(&body)
                .send()
                .await?;

            let status = response.status();
            if !status.is_success() {
                let retry_after_ms = crate::parse_retry_after_ms(response.headers()).unwrap_or(5000);
                let body_text = response.text().await.unwrap_or_default();
                if status.as_u16() == 429 {
                    return Err(ProviderError::RateLimited {
                        retry_after_ms,
                        message: crate::non_empty_rate_limit_message(body_text),
                    });
                }
                return Err(ProviderError::Api {
                    status: status.as_u16(),
                    message: body_text,
                });
            }
            Ok(response)
        })
        .await?;

        let (tx, rx) = mpsc::channel(64);
        let client = client.clone();
        let url_clone = url.clone();
        let headers_clone = {
            let mut h = HeaderMap::new();
            h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            h.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", access_token))
                    .map_err(|e| ProviderError::Connection(format!("Header error: {}", e)))?,
            );
            h
        };

        // Vertex uses standard SSE (same as Anthropic)
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
                            match crate::retry::send_and_check(
                                &client,
                                &url_clone,
                                &headers_clone,
                                &body,
                            )
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

// --- Internal types ---

#[derive(Debug, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Debug, Serialize)]
struct JwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: u64,
    exp: u64,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

#[derive(Debug, Deserialize)]
struct AdcCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

/// Build GcpAuth from nomi-config's VertexConfig
pub fn auth_from_config(vc: &nomi_config::config::VertexConfig) -> GcpAuth {
    if let Some(creds_file) = &vc.credentials_file {
        GcpAuth::ServiceAccount {
            key_file: creds_file.clone(),
        }
    } else {
        GcpAuth::ApplicationDefault
    }
}
