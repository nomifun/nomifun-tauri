//! Task → HTTP endpoint / request-shape resolver. The single authority for
//! "given a provider (platform, base_url, is_full_url) + a [`ModelTask`] +
//! per-model params, where and how do we call it". Both the health probe and
//! (eventually) the media dispatch consult this so the endpoint decision lives
//! in exactly one place.
//!
//! Resolution rules (first match wins):
//! 1. `params.endpoint` string override — a full URL (`http(s)://…`) is used
//!    verbatim; a leading-slash path is appended to the trimmed base. This is
//!    the zero-code escape hatch for non-standard multimodal providers.
//! 2. `is_full_url` — the provider's `base_url` is already the complete endpoint.
//! 3. Convention — `{trimmed_base}{conventional_path(task)}`. The provider
//!    `base_url` is assumed to already carry any version prefix (e.g.
//!    `https://api.stepfun.com/step_plan/v1`), matching how the working chat
//!    path composes URLs today.
//!
//! `params.request_shape` (`"json"` | `"multipart"`) overrides the default shape.

use serde::{Deserialize, Serialize};

use crate::model_task::ModelTask;

/// How the request body is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestShape {
    /// `application/json` body.
    Json,
    /// `multipart/form-data` body (file uploads: image edits, audio transcription).
    Multipart,
}

/// The resolved call target for one (provider, task).
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchTarget {
    pub url: String,
    /// HTTP method — currently always `POST` for every task, kept explicit for clarity.
    pub method: String,
    pub shape: RequestShape,
}

/// The conventional sub-path + default shape for a task (OpenAI-compatible).
fn conventional(task: ModelTask) -> (&'static str, RequestShape) {
    match task {
        ModelTask::Chat => ("/chat/completions", RequestShape::Json),
        ModelTask::ImageGeneration => ("/images/generations", RequestShape::Json),
        ModelTask::ImageEdit => ("/images/edits", RequestShape::Multipart),
        ModelTask::VideoGeneration => ("/videos", RequestShape::Json),
        ModelTask::SpeechSynthesis => ("/audio/speech", RequestShape::Json),
        ModelTask::SpeechRecognition => ("/audio/transcriptions", RequestShape::Multipart),
        ModelTask::Embedding => ("/embeddings", RequestShape::Json),
        ModelTask::Rerank => ("/rerank", RequestShape::Json),
    }
}

fn params_str<'a>(params: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

/// Resolve the endpoint + request shape for a (provider, task).
pub fn resolve_dispatch_target(
    _platform: &str,
    base_url: &str,
    is_full_url: bool,
    task: ModelTask,
    params: &serde_json::Value,
) -> DispatchTarget {
    let trimmed = base_url.trim().trim_end_matches('/');
    let (conv_path, conv_shape) = conventional(task);

    // 1. Per-model endpoint override (escape hatch for non-standard providers).
    let url = if let Some(ep) = params_str(params, "endpoint") {
        if ep.starts_with("http://") || ep.starts_with("https://") {
            ep.to_string()
        } else {
            let path = if ep.starts_with('/') { ep.to_string() } else { format!("/{ep}") };
            format!("{trimmed}{path}")
        }
    } else if is_full_url {
        // 2. base_url is already the complete endpoint.
        trimmed.to_string()
    } else {
        // 3. Convention. Normalize to a single `/v1` version root (mirrors
        // nomifun-creation's `openai_versioned_base`): a base is tolerated with
        // or without a trailing `/v1`, and StepFun's `.../step_plan/v1` collapses
        // to itself. All OpenAI-compatible task endpoints live under `/v1`.
        let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
        format!("{root}/v1{conv_path}")
    };

    // request_shape override.
    let shape = match params_str(params, "request_shape") {
        Some("multipart") => RequestShape::Multipart,
        Some("json") => RequestShape::Json,
        _ => conv_shape,
    };

    DispatchTarget { url, method: "POST".to_string(), shape }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NONE: serde_json::Value = serde_json::Value::Null;

    #[test]
    fn stepfun_plan_image_generation_matches_tested_url() {
        // The exact URL confirmed working (HTTP 200) against StepFun.
        let t = resolve_dispatch_target(
            "stepfun-plan",
            "https://api.stepfun.com/step_plan/v1",
            false,
            ModelTask::ImageGeneration,
            &NONE,
        );
        assert_eq!(t.url, "https://api.stepfun.com/step_plan/v1/images/generations");
        assert_eq!(t.shape, RequestShape::Json);
    }

    #[test]
    fn image_edit_is_multipart() {
        let t = resolve_dispatch_target(
            "stepfun-plan",
            "https://api.stepfun.com/step_plan/v1/",
            false,
            ModelTask::ImageEdit,
            &NONE,
        );
        assert_eq!(t.url, "https://api.stepfun.com/step_plan/v1/images/edits");
        assert_eq!(t.shape, RequestShape::Multipart);
    }

    #[test]
    fn chat_convention() {
        let t = resolve_dispatch_target("openai", "https://api.openai.com/v1", false, ModelTask::Chat, &NONE);
        assert_eq!(t.url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn is_full_url_uses_base_verbatim() {
        let t = resolve_dispatch_target(
            "custom",
            "https://example.com/my/exact/endpoint",
            true,
            ModelTask::ImageGeneration,
            &NONE,
        );
        assert_eq!(t.url, "https://example.com/my/exact/endpoint");
    }

    #[test]
    fn endpoint_override_full_url_wins() {
        let params = json!({ "endpoint": "https://api.deepgram.com/v1/listen" });
        let t = resolve_dispatch_target(
            "deepgram",
            "https://api.deepgram.com",
            false,
            ModelTask::SpeechRecognition,
            &params,
        );
        assert_eq!(t.url, "https://api.deepgram.com/v1/listen");
    }

    #[test]
    fn endpoint_override_path_appended() {
        let params = json!({ "endpoint": "/custom/gen" });
        let t = resolve_dispatch_target(
            "custom",
            "https://x.test/v2/",
            false,
            ModelTask::ImageGeneration,
            &params,
        );
        assert_eq!(t.url, "https://x.test/v2/custom/gen");
    }

    #[test]
    fn request_shape_override() {
        let params = json!({ "request_shape": "multipart" });
        let t = resolve_dispatch_target("x", "https://x.test/v1", false, ModelTask::Embedding, &params);
        assert_eq!(t.shape, RequestShape::Multipart);
    }
}
