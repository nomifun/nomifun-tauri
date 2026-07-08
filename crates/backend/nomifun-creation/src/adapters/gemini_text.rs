//! `gemini_text` — Google Gemini text generation via `:generateContent`.
//!
//! Mirrors [`super::gemini_image`]'s request skeleton but requests text only
//! (no `responseModalities: IMAGE`): the prompt is the leading text part, input
//! assets attach as `inline_data` (images) or extra text parts, and an optional
//! `params.system` becomes `systemInstruction`. The reply is read from
//! `candidates[].content.parts[].text` and returned inline as
//! `text/plain; charset=utf-8`. Synchronous — [`SubmitAck::Done`].

use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

use crate::adapters::openai_chat::text_asset;
use crate::adapters::{encode_b64, error_from_response, gemini_generate_url, net_err, param_prompt};
use crate::provider::{InputAsset, MediaProvider, SubmitAck, SubmitRequest};
use crate::types::{CreationError, MediaCapability};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) struct GeminiTextAdapter {
    http: reqwest::Client,
}

impl GeminiTextAdapter {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl MediaProvider for GeminiTextAdapter {
    fn id(&self) -> &'static str {
        "gemini_text"
    }

    fn supports(&self, cap: MediaCapability) -> bool {
        matches!(cap, MediaCapability::Text)
    }

    async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        let url = gemini_generate_url(&req.provider, &req.model);
        let body = build_gemini_text_body(&req.params, &req.inputs);

        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &req.provider.api_key)
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| CreationError::provider_error(format!("invalid gemini JSON: {e}")))?;
        let text = parse_gemini_text(&value)?;
        Ok(SubmitAck::Done(vec![text_asset(text)]))
    }

    async fn poll(&self, _remote: &str, _req: &SubmitRequest) -> Result<crate::provider::PollResult, CreationError> {
        Err(CreationError::config("gemini_text is synchronous and has no poll step"))
    }
}

/// Build the `:generateContent` body for text generation. Pure — unit tested.
///
/// - The prompt is the leading text part; each input attaches as `inline_data`
///   (images) or a text part (`text/*` inputs).
/// - `params.system` (non-empty) → `systemInstruction`.
/// - `params.max_tokens` (number) → `generationConfig.maxOutputTokens`, else omitted.
pub(crate) fn build_gemini_text_body(params: &Value, inputs: &[InputAsset]) -> Value {
    let mut parts: Vec<Value> = vec![json!({"text": param_prompt(params)})];
    for input in inputs {
        if input.mime.starts_with("text/") {
            parts.push(json!({"text": String::from_utf8_lossy(&input.bytes)}));
        } else {
            parts.push(json!({
                "inline_data": { "mime_type": input.mime, "data": encode_b64(&input.bytes) }
            }));
        }
    }
    let mut body = json!({ "contents": [{ "parts": parts }] });
    if let Some(system) =
        params.get("system").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
    {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if let Some(max) = params.get("max_tokens").and_then(|v| v.as_u64()) {
        body["generationConfig"] = json!({ "maxOutputTokens": max });
    }
    body
}

/// Concatenate `candidates[].content.parts[].text`. Surfaces a
/// `promptFeedback.blockReason` when the model returned no text. Pure — unit tested.
pub(crate) fn parse_gemini_text(value: &Value) -> Result<String, CreationError> {
    let candidates = value
        .get("candidates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CreationError::provider_error("gemini response missing 'candidates'"))?;

    let mut out = String::new();
    for cand in candidates {
        let Some(parts) = cand.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) else {
            continue;
        };
        for part in parts {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                out.push_str(t);
            }
        }
    }

    if out.trim().is_empty() {
        let reason = value
            .get("promptFeedback")
            .and_then(|f| f.get("blockReason"))
            .and_then(|v| v.as_str())
            .unwrap_or("no text parts in response");
        return Err(CreationError::provider_error(format!("gemini produced no text: {reason}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_concatenates_text_parts() {
        let v = json!({
            "candidates": [{ "content": { "parts": [
                {"text": "gemini says "},
                {"text": "hi"}
            ]}}]
        });
        assert_eq!(parse_gemini_text(&v).unwrap(), "gemini says hi");
    }

    #[test]
    fn parse_no_text_surfaces_block_reason() {
        let v = json!({"candidates": [], "promptFeedback": {"blockReason": "SAFETY"}});
        let err = parse_gemini_text(&v).unwrap_err();
        assert!(err.message.contains("SAFETY"), "{}", err.message);
    }

    #[test]
    fn parse_missing_candidates_errors() {
        assert!(parse_gemini_text(&json!({})).is_err());
    }

    #[test]
    fn body_prompt_only_has_no_config() {
        let body = build_gemini_text_body(&json!({"prompt": "greet me"}), &[]);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "greet me");
        assert!(body.get("systemInstruction").is_none());
        assert!(body.get("generationConfig").is_none());
    }

    #[test]
    fn body_carries_system_and_max_tokens_and_inputs() {
        let inputs = vec![
            InputAsset { asset_id: "a".into(), role: "reference".into(), bytes: b"hi".to_vec(), mime: "image/png".into() },
            InputAsset { asset_id: "b".into(), role: "reference".into(), bytes: b"notes".to_vec(), mime: "text/plain".into() },
        ];
        let body = build_gemini_text_body(&json!({"prompt": "p", "system": " sys ", "max_tokens": 64}), &inputs);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["text"], "p");
        // image → inline_data ("aGk=" is base64("hi"))
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "aGk=");
        // text input → text part
        assert_eq!(parts[2]["text"], "notes");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
    }
}
