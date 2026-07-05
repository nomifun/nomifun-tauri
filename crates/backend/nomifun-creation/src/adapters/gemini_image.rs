//! `gemini_image` — Google Gemini image generation via `:generateContent`.
//!
//! `POST {base}/v1beta/models/{model}:generateContent` with an `x-goog-api-key`
//! header; the request `contents.parts` carry the prompt text plus any input
//! images as `inline_data`, and `generationConfig.responseModalities` requests
//! `["TEXT","IMAGE"]`. The response's `candidates[].content.parts[].inlineData`
//! carry the generated image(s). Synchronous — [`SubmitAck::Done`].

use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

use crate::adapters::{encode_b64, error_from_response, gemini_generate_url, net_err, param_prompt};
use crate::provider::{MediaProvider, ProducedAsset, ProducedData, SubmitAck, SubmitRequest};
use crate::types::{CreationError, MediaCapability};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) struct GeminiImageAdapter {
    http: reqwest::Client,
}

impl GeminiImageAdapter {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl MediaProvider for GeminiImageAdapter {
    fn id(&self) -> &'static str {
        "gemini_image"
    }

    fn supports(&self, cap: MediaCapability) -> bool {
        matches!(cap, MediaCapability::T2i | MediaCapability::I2i)
    }

    async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        let url = gemini_generate_url(&req.provider, &req.model);

        // parts: [ {text}, {inline_data}, ... ]
        let mut parts: Vec<Value> = vec![json!({"text": param_prompt(&req.params)})];
        for input in &req.inputs {
            parts.push(json!({
                "inline_data": {
                    "mime_type": input.mime,
                    "data": encode_b64(&input.bytes),
                }
            }));
        }
        let body = json!({
            "contents": [{ "parts": parts }],
            "generationConfig": { "responseModalities": ["TEXT", "IMAGE"] }
        });

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
        Ok(SubmitAck::Done(parse_gemini_response(&value)?))
    }

    async fn poll(&self, _remote_task_id: &str, _req: &SubmitRequest) -> Result<crate::provider::PollResult, CreationError> {
        Err(CreationError::config("gemini_image is synchronous and has no poll step"))
    }
}

/// Parse `candidates[].content.parts[].inlineData{mimeType,data}` into image
/// artifacts. Accepts both camelCase (`inlineData`/`mimeType`) and snake_case
/// (`inline_data`/`mime_type`) shapes. Pure — unit tested.
pub(crate) fn parse_gemini_response(value: &Value) -> Result<Vec<ProducedAsset>, CreationError> {
    let candidates = value
        .get("candidates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CreationError::provider_error("gemini response missing 'candidates'"))?;

    let mut out = Vec::new();
    for cand in candidates {
        let Some(parts) = cand.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) else {
            continue;
        };
        for part in parts {
            let inline = part.get("inlineData").or_else(|| part.get("inline_data"));
            let Some(inline) = inline else { continue };
            let Some(data) = inline.get("data").and_then(|v| v.as_str()) else { continue };
            let bytes = super::decode_b64(data)
                .ok_or_else(|| CreationError::provider_error("gemini inlineData is not valid base64"))?;
            let mime = inline
                .get("mimeType")
                .or_else(|| inline.get("mime_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            out.push(ProducedAsset { data: ProducedData::Bytes(bytes), mime: Some(mime) });
        }
    }

    if out.is_empty() {
        // Surface any prompt-feedback / block reason if the model refused.
        let reason = value
            .get("promptFeedback")
            .and_then(|f| f.get("blockReason"))
            .and_then(|v| v.as_str())
            .unwrap_or("no image parts in response");
        return Err(CreationError::provider_error(format!("gemini produced no image: {reason}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_camel_and_snake_inline_data() {
        let v = json!({
            "candidates": [{
                "content": { "parts": [
                    {"text": "here you go"},
                    {"inlineData": {"mimeType": "image/png", "data": "aGk="}}
                ]}
            }]
        });
        let out = parse_gemini_response(&v).unwrap();
        assert_eq!(out.len(), 1);
        match &out[0].data {
            ProducedData::Bytes(b) => assert_eq!(b, b"hi"),
            _ => panic!("expected bytes"),
        }
        assert_eq!(out[0].mime.as_deref(), Some("image/png"));

        let v2 = json!({
            "candidates": [{ "content": { "parts": [
                {"inline_data": {"mime_type": "image/jpeg", "data": "aGk="}}
            ]}}]
        });
        let out2 = parse_gemini_response(&v2).unwrap();
        assert_eq!(out2[0].mime.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn parse_no_image_surfaces_block_reason() {
        let v = json!({"candidates": [], "promptFeedback": {"blockReason": "SAFETY"}});
        let err = parse_gemini_response(&v).unwrap_err();
        assert!(err.message.contains("SAFETY"), "{}", err.message);
    }

    #[test]
    fn parse_missing_candidates_errors() {
        assert!(parse_gemini_response(&json!({})).is_err());
    }
}
