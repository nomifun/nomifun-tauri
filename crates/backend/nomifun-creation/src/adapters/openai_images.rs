//! `openai_images` — OpenAI-compatible synchronous image generation/editing.
//!
//! - t2i → `POST {base}/v1/images/generations` (JSON body).
//! - i2i / inpaint → `POST {base}/v1/images/edits` (multipart: `image`(s) +
//!   optional `mask` + prompt/n/size).
//!
//! Both are synchronous — [`SubmitAck::Done`] carries the artifacts inline.
//! `response_format=b64_json` is requested; the parser also tolerates providers
//! that return a `url` instead (the engine fetches it).

use async_trait::async_trait;
use nomifun_api_types::{resolve_dispatch_target, ModelTask};
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use std::time::Duration;

use crate::adapters::{error_from_response, net_err, param_count, param_prompt, param_size};
use crate::provider::{MediaProvider, ProducedAsset, ProducedData, SubmitAck, SubmitRequest};
use crate::types::{CreationError, MediaCapability};

/// Generous per-call ceiling: image generation is often multi-second.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) struct OpenAiImagesAdapter {
    http: reqwest::Client,
}

impl OpenAiImagesAdapter {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn submit_generations(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        let url = resolve_dispatch_target(
            &req.provider.platform,
            &req.provider.base_url,
            req.provider.is_full_url,
            ModelTask::ImageGeneration,
            &req.params,
        )
        .url;
        let mut body = json!({
            "model": req.model,
            "prompt": param_prompt(&req.params),
            "n": param_count(&req.params),
            "response_format": "b64_json",
        });
        if let Some(size) = param_size(&req.params) {
            body["size"] = Value::String(size);
        }
        if let Some(quality) = req.params.get("quality").and_then(|v| v.as_str()) {
            body["quality"] = Value::String(quality.to_string());
        }

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", req.provider.api_key))
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let value: Value = resp.json().await.map_err(|e| CreationError::provider_error(format!("invalid images JSON: {e}")))?;
        Ok(SubmitAck::Done(parse_images_response(&value)?))
    }

    async fn submit_edits(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        let url = resolve_dispatch_target(
            &req.provider.platform,
            &req.provider.base_url,
            req.provider.is_full_url,
            ModelTask::ImageEdit,
            &req.params,
        )
        .url;

        let images: Vec<_> = req.inputs.iter().filter(|i| i.role != "mask").collect();
        if images.is_empty() {
            return Err(CreationError::new(
                "bad_input",
                "images/edits requires at least one input image (role != 'mask')",
            ));
        }
        // Single image → `image`; multiple → `image[]` (gpt-image-1 multi-ref).
        let image_field = if images.len() == 1 { "image" } else { "image[]" };

        let mut form = Form::new()
            .text("model", req.model.clone())
            .text("prompt", param_prompt(&req.params))
            .text("n", param_count(&req.params).to_string());
        if let Some(size) = param_size(&req.params) {
            form = form.text("size", size);
        }
        for (idx, input) in images.iter().enumerate() {
            let part = Part::bytes(input.bytes.clone())
                .file_name(format!("image_{idx}.{}", ext_for_mime(&input.mime)))
                .mime_str(&input.mime)
                .map_err(|e| CreationError::provider_error(format!("invalid image mime: {e}")))?;
            form = form.part(image_field, part);
        }
        if let Some(mask) = req.inputs.iter().find(|i| i.role == "mask") {
            let part = Part::bytes(mask.bytes.clone())
                .file_name("mask.png")
                .mime_str(&mask.mime)
                .map_err(|e| CreationError::provider_error(format!("invalid mask mime: {e}")))?;
            form = form.part("mask", part);
        }

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", req.provider.api_key))
            .timeout(REQUEST_TIMEOUT)
            .multipart(form)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let value: Value = resp.json().await.map_err(|e| CreationError::provider_error(format!("invalid images JSON: {e}")))?;
        Ok(SubmitAck::Done(parse_images_response(&value)?))
    }
}

#[async_trait]
impl MediaProvider for OpenAiImagesAdapter {
    fn id(&self) -> &'static str {
        "openai_images"
    }

    fn supports(&self, cap: MediaCapability) -> bool {
        matches!(cap, MediaCapability::T2i | MediaCapability::I2i | MediaCapability::Inpaint)
    }

    async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        match req.capability {
            MediaCapability::T2i => self.submit_generations(req).await,
            MediaCapability::I2i | MediaCapability::Inpaint => self.submit_edits(req).await,
            other => Err(CreationError::new(
                "unsupported_capability",
                format!("openai_images cannot serve {}", other.as_str()),
            )),
        }
    }

    async fn poll(&self, _remote_task_id: &str, _req: &SubmitRequest) -> Result<crate::provider::PollResult, CreationError> {
        // Synchronous protocol: submit always returns Done, so poll is never
        // reached. Guard against misuse with a clear error.
        Err(CreationError::config("openai_images is synchronous and has no poll step"))
    }
}

/// Parse an OpenAI images response body (`{ data: [ { b64_json?, url? } ] }`)
/// into artifacts, preferring inline base64 over a URL. Pure — unit tested with
/// fixtures.
pub(crate) fn parse_images_response(value: &Value) -> Result<Vec<ProducedAsset>, CreationError> {
    let data = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CreationError::provider_error("images response missing 'data' array"))?;
    if data.is_empty() {
        return Err(CreationError::provider_error("images response 'data' array is empty"));
    }
    let mut out = Vec::with_capacity(data.len());
    for item in data {
        if let Some(b64) = item.get("b64_json").and_then(|v| v.as_str()) {
            let bytes = super::decode_b64(b64)
                .ok_or_else(|| CreationError::provider_error("images b64_json is not valid base64"))?;
            out.push(ProducedAsset { data: ProducedData::Bytes(bytes), mime: Some("image/png".into()) });
        } else if let Some(url) = item.get("url").and_then(|v| v.as_str()) {
            out.push(ProducedAsset { data: ProducedData::Url(url.to_string()), mime: None });
        } else {
            return Err(CreationError::provider_error("images data item has neither b64_json nor url"));
        }
    }
    Ok(out)
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_b64_response() {
        // "aGk=" is base64("hi").
        let v = json!({"data": [{"b64_json": "aGk="}]});
        let out = parse_images_response(&v).unwrap();
        assert_eq!(out.len(), 1);
        match &out[0].data {
            ProducedData::Bytes(b) => assert_eq!(b, b"hi"),
            _ => panic!("expected bytes"),
        }
        assert_eq!(out[0].mime.as_deref(), Some("image/png"));
    }

    #[test]
    fn parse_url_response() {
        let v = json!({"data": [{"url": "https://cdn/x.png"}, {"url": "https://cdn/y.png"}]});
        let out = parse_images_response(&v).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0].data, ProducedData::Url(u) if u == "https://cdn/x.png"));
    }

    #[test]
    fn parse_errors_on_empty_or_missing() {
        assert!(parse_images_response(&json!({})).is_err());
        assert!(parse_images_response(&json!({"data": []})).is_err());
        assert!(parse_images_response(&json!({"data": [{}]})).is_err());
        assert!(parse_images_response(&json!({"data": [{"b64_json": "!!!not base64!!!"}]})).is_err());
    }

    #[test]
    fn ext_mapping() {
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("image/png"), "png");
        assert_eq!(ext_for_mime("application/octet-stream"), "png");
    }
}
