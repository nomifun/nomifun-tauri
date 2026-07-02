//! Outbound webhook delivery. v1 supports Lark/飞书 custom bots.
//!
//! Signing + payload construction are pure functions so they can be unit-tested
//! without a live HTTP server; `send_card` performs the actual POST.

use std::sync::Arc;

use base64::Engine;
use hmac::{Hmac, Mac};
use nomifun_api_types::WebhookPlatform;
use serde_json::{Value, json};
use sha2::Sha256;

use crate::error::WebhookError;

type HmacSha256 = Hmac<Sha256>;

/// Abstraction over a webhook platform's "send a notification card" operation.
/// Kept as a trait so the completion notifier + tests can swap in a mock, and so
/// future platforms can be added without touching callers.
#[async_trait::async_trait]
pub trait WebhookSender: Send + Sync {
    /// Send a titled card with `(label, value)` field rows to `url`. When
    /// `secret` is set, the request is signed (Lark 加签). `platform` selects
    /// the payload shape (Lark interactive card / Slack text / generic HTTP JSON).
    async fn send_card(
        &self,
        platform: WebhookPlatform,
        url: &str,
        secret: Option<&str>,
        title: &str,
        fields: &[(String, String)],
    ) -> Result<(), WebhookError>;
}

/// Platform-dispatching sender: builds the right payload per platform
/// (Lark interactive card / Slack text / generic HTTP JSON) and POSTs it.
#[derive(Clone)]
pub struct DefaultWebhookSender {
    client: HttpClientFactory,
}

type HttpClientFactory = Arc<dyn Fn() -> reqwest::Client + Send + Sync>;

impl Default for DefaultWebhookSender {
    fn default() -> Self {
        Self {
            client: Arc::new(nomifun_net::http_client),
        }
    }
}

impl DefaultWebhookSender {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client: Arc::new(move || client.clone()),
        }
    }

    fn client(&self) -> reqwest::Client {
        (self.client)()
    }
}

/// Compute the Lark custom-bot signature: `base64(HMAC-SHA256(key = "{ts}\n{secret}", msg = ""))`.
pub fn lark_sign(secret: &str, timestamp: i64) -> Result<String, WebhookError> {
    let string_to_sign = format!("{timestamp}\n{secret}");
    let mut mac =
        HmacSha256::new_from_slice(string_to_sign.as_bytes()).map_err(|e| WebhookError::Sign(e.to_string()))?;
    mac.update(b"");
    let code = mac.finalize().into_bytes();
    Ok(base64::engine::general_purpose::STANDARD.encode(code))
}

/// Build the Lark interactive-card message body (without signing fields).
pub fn build_lark_card(title: &str, fields: &[(String, String)]) -> Value {
    let elements: Vec<Value> = fields
        .iter()
        .map(|(label, value)| {
            json!({
                "tag": "div",
                "text": { "tag": "lark_md", "content": format!("**{label}**\n{value}") }
            })
        })
        .collect();
    json!({
        "msg_type": "interactive",
        "card": {
            "config": { "wide_screen_mode": true },
            "header": {
                "title": { "tag": "plain_text", "content": title },
                "template": "blue"
            },
            "elements": elements
        }
    })
}

/// Build the full request body, adding `timestamp`/`sign` when a secret is set.
pub fn build_lark_body(
    secret: Option<&str>,
    timestamp: i64,
    title: &str,
    fields: &[(String, String)],
) -> Result<Value, WebhookError> {
    let mut body = build_lark_card(title, fields);
    if let Some(secret) = secret.filter(|s| !s.is_empty()) {
        let sign = lark_sign(secret, timestamp)?;
        body["timestamp"] = json!(timestamp.to_string());
        body["sign"] = json!(sign);
    }
    Ok(body)
}

/// Build a Slack incoming-webhook body: a single text blob with title + field lines.
pub fn build_slack_body(title: &str, fields: &[(String, String)]) -> Value {
    let mut text = format!("*{title}*");
    for (label, value) in fields {
        text.push_str(&format!("\n*{label}*: {value}"));
    }
    json!({ "text": text })
}

/// Build a generic HTTP JSON body: structured title + fields so any consumer can parse it.
pub fn build_http_body(title: &str, fields: &[(String, String)]) -> Value {
    let field_objs: Vec<Value> = fields
        .iter()
        .map(|(label, value)| json!({ "label": label, "value": value }))
        .collect();
    json!({ "title": title, "fields": field_objs })
}

#[async_trait::async_trait]
impl WebhookSender for DefaultWebhookSender {
    async fn send_card(
        &self,
        platform: WebhookPlatform,
        url: &str,
        secret: Option<&str>,
        title: &str,
        fields: &[(String, String)],
    ) -> Result<(), WebhookError> {
        let body = match platform {
            WebhookPlatform::Lark => {
                let timestamp = chrono::Utc::now().timestamp();
                build_lark_body(secret, timestamp, title, fields)?
            }
            WebhookPlatform::Slack => build_slack_body(title, fields),
            WebhookPlatform::Http => build_http_body(title, fields),
        };
        let client = self.client();
        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| WebhookError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(WebhookError::Remote(format!("HTTP {status}: {text}")));
        }
        // Lark replies {"code":0,...} (or legacy {"StatusCode":0,...}) on success;
        // Slack/HTTP treat any 2xx as success (response body is free-form).
        if matches!(platform, WebhookPlatform::Lark) {
            let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
            let code = parsed
                .get("code")
                .and_then(Value::as_i64)
                .or_else(|| parsed.get("StatusCode").and_then(Value::as_i64))
                .unwrap_or(0);
            if code != 0 {
                return Err(WebhookError::Remote(format!("lark code {code}: {text}")));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_deterministic_and_base64() {
        let a = lark_sign("secret", 1_700_000_000).unwrap();
        let b = lark_sign("secret", 1_700_000_000).unwrap();
        assert_eq!(a, b);
        assert!(!a.is_empty());
        // valid base64 decodes to 32 bytes (sha256 output)
        let decoded = base64::engine::general_purpose::STANDARD.decode(&a).unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn sign_changes_with_timestamp() {
        let a = lark_sign("secret", 1).unwrap();
        let b = lark_sign("secret", 2).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn body_without_secret_has_no_sign() {
        let fields = [("需求名".to_string(), "build X".to_string())];
        let body = build_lark_body(None, 123, "title", &fields).unwrap();
        assert_eq!(body["msg_type"], "interactive");
        assert!(body.get("sign").is_none());
        assert!(body.get("timestamp").is_none());
        let content = body["card"]["elements"][0]["text"]["content"].as_str().unwrap();
        assert!(content.contains("需求名"));
        assert!(content.contains("build X"));
    }

    #[test]
    fn body_with_secret_includes_sign_and_timestamp() {
        let body = build_lark_body(Some("s"), 999, "t", &[]).unwrap();
        assert_eq!(body["timestamp"], "999");
        assert!(body["sign"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn empty_secret_is_treated_as_unsigned() {
        let body = build_lark_body(Some(""), 999, "t", &[]).unwrap();
        assert!(body.get("sign").is_none());
    }

    #[test]
    fn slack_body_has_text_with_title_and_fields() {
        let fields = [("需求名".to_string(), "build X".to_string())];
        let body = build_slack_body("标题", &fields);
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("标题"));
        assert!(text.contains("需求名"));
        assert!(text.contains("build X"));
    }

    #[test]
    fn http_body_is_structured_json() {
        let fields = [("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())];
        let body = build_http_body("T", &fields);
        assert_eq!(body["title"], "T");
        assert_eq!(body["fields"][0]["label"], "a");
        assert_eq!(body["fields"][0]["value"], "1");
        assert_eq!(body["fields"][1]["label"], "b");
    }
}
