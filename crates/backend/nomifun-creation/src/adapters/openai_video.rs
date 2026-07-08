//! `openai_video` — OpenAI-compatible asynchronous video generation.
//!
//! - submit → `POST {base}/v1/videos` (multipart: model/prompt/seconds/size +
//!   optional `input_reference` for i2v). Returns a remote job `{id,status}` →
//!   [`SubmitAck::Pending`].
//! - poll → `GET {base}/v1/videos/{id}`; on `completed`, fetch the bytes from
//!   `GET {base}/v1/videos/{id}/content` → [`PollResult::Done`]; on `failed`,
//!   [`PollResult::Failed`].

use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use std::time::Duration;

use crate::adapters::{
    MAX_ARTIFACT_BYTES, error_from_response, net_err, openai_versioned_base, param_prompt, param_size,
    read_body_capped,
};
use crate::provider::{MediaProvider, PollResult, ProducedAsset, ProducedData, SubmitAck, SubmitRequest};
use crate::types::{CreationError, MediaCapability};

const SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// Downloading a finished video can be large; allow more headroom.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct OpenAiVideoAdapter {
    http: reqwest::Client,
}

impl OpenAiVideoAdapter {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl MediaProvider for OpenAiVideoAdapter {
    fn id(&self) -> &'static str {
        "openai_video"
    }

    fn supports(&self, cap: MediaCapability) -> bool {
        matches!(cap, MediaCapability::T2v | MediaCapability::I2v)
    }

    async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        let url = format!("{}/videos", openai_versioned_base(&req.provider));

        let mut form = Form::new()
            .text("model", req.model.clone())
            .text("prompt", param_prompt(&req.params));
        if let Some(seconds) = req.params.get("seconds").and_then(json_number_or_string) {
            form = form.text("seconds", seconds);
        }
        if let Some(size) = param_size(&req.params) {
            form = form.text("size", size);
        }
        // i2v reference frame — the first reference/first_frame input.
        if let Some(reference) = req
            .inputs
            .iter()
            .find(|i| matches!(i.role.as_str(), "reference" | "first_frame"))
            .or_else(|| req.inputs.first())
        {
            let part = Part::bytes(reference.bytes.clone())
                .file_name("input_reference")
                .mime_str(&reference.mime)
                .map_err(|e| CreationError::provider_error(format!("invalid reference mime: {e}")))?;
            form = form.part("input_reference", part);
        }

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", req.provider.api_key))
            .timeout(SUBMIT_TIMEOUT)
            .multipart(form)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| CreationError::provider_error(format!("invalid videos JSON: {e}")))?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CreationError::provider_error("videos submit response missing 'id'"))?;
        Ok(SubmitAck::Pending { remote_task_id: id.to_string() })
    }

    async fn poll(&self, remote_task_id: &str, req: &SubmitRequest) -> Result<PollResult, CreationError> {
        let base = openai_versioned_base(&req.provider);
        let url = format!("{base}/videos/{remote_task_id}");
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", req.provider.api_key))
            .timeout(POLL_TIMEOUT)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| CreationError::provider_error(format!("invalid videos status JSON: {e}")))?;

        match parse_video_status(&value) {
            VideoStatus::Pending => Ok(PollResult::Pending),
            VideoStatus::Failed(msg) => Ok(PollResult::Failed(CreationError::provider_error(msg))),
            VideoStatus::Completed => {
                let content_url = format!("{base}/videos/{remote_task_id}/content");
                let resp = self
                    .http
                    .get(&content_url)
                    .header("Authorization", format!("Bearer {}", req.provider.api_key))
                    .timeout(CONTENT_TIMEOUT)
                    .send()
                    .await
                    .map_err(net_err)?;
                if !resp.status().is_success() {
                    return Err(error_from_response(resp).await);
                }
                let mime = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "video/mp4".to_string());
                let bytes = read_body_capped(resp, MAX_ARTIFACT_BYTES).await?;
                Ok(PollResult::Done(vec![ProducedAsset {
                    data: ProducedData::Bytes(bytes),
                    mime: Some(mime),
                }]))
            }
        }
    }
}

/// The distilled state of a video job.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VideoStatus {
    Pending,
    Completed,
    Failed(String),
}

/// Map a videos status body to a [`VideoStatus`]. Tolerant of the common status
/// vocabulary across OpenAI-compatible video APIs. Pure — unit tested.
pub(crate) fn parse_video_status(value: &Value) -> VideoStatus {
    let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
    match status.as_str() {
        "completed" | "succeeded" | "success" | "done" => VideoStatus::Completed,
        "failed" | "error" | "cancelled" | "canceled" => {
            let msg = value
                .get("error")
                .and_then(|e| e.get("message").and_then(|m| m.as_str()).or_else(|| e.as_str()))
                .unwrap_or("video generation failed")
                .to_string();
            VideoStatus::Failed(msg)
        }
        // "queued" | "in_progress" | "running" | "processing" | "" → keep waiting.
        _ => VideoStatus::Pending,
    }
}

/// Render a JSON number or string param as a plain string for multipart text.
fn json_number_or_string(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_completed_variants() {
        for s in ["completed", "succeeded", "done"] {
            assert_eq!(parse_video_status(&json!({"status": s})), VideoStatus::Completed);
        }
    }

    #[test]
    fn status_pending_variants() {
        for s in ["queued", "in_progress", "running", "processing", ""] {
            assert_eq!(parse_video_status(&json!({"status": s})), VideoStatus::Pending);
        }
        assert_eq!(parse_video_status(&json!({})), VideoStatus::Pending);
    }

    #[test]
    fn status_failed_carries_message() {
        let v = json!({"status": "failed", "error": {"message": "moderation blocked"}});
        assert_eq!(parse_video_status(&v), VideoStatus::Failed("moderation blocked".into()));
        // string error form
        let v2 = json!({"status": "error", "error": "boom"});
        assert_eq!(parse_video_status(&v2), VideoStatus::Failed("boom".into()));
        // no detail → default message
        let v3 = json!({"status": "failed"});
        assert_eq!(parse_video_status(&v3), VideoStatus::Failed("video generation failed".into()));
    }

    #[test]
    fn number_or_string_render() {
        assert_eq!(json_number_or_string(&json!(8)), Some("8".into()));
        assert_eq!(json_number_or_string(&json!("6")), Some("6".into()));
        assert_eq!(json_number_or_string(&json!("  ")), None);
        assert_eq!(json_number_or_string(&json!(null)), None);
    }
}
