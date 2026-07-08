//! `openai_chat` — OpenAI-compatible synchronous chat/completions (LLM text).
//!
//! `POST {base}/v1/chat/completions` (non-streaming). The prompt is the user
//! message; when the task carries input assets the user content becomes a
//! multimodal segment array (image inputs → `image_url` data URLs, text inputs
//! → text segments). An optional `params.system` prepends a system message;
//! `params.max_tokens` is forwarded only when present. The reply is read from
//! `choices[0].message.content` and returned inline as UTF-8 text
//! (`text/plain; charset=utf-8`). Synchronous — [`SubmitAck::Done`].

use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

use crate::adapters::{encode_b64, error_from_response, net_err, openai_versioned_base, param_prompt};
use crate::provider::{InputAsset, MediaProvider, ProducedAsset, ProducedData, SubmitAck, SubmitRequest};
use crate::types::{CreationError, MediaCapability};

/// The MIME stamped on produced text artifacts (the bridge keys its text-asset
/// special case off a `text/plain` prefix).
pub(crate) const TEXT_MIME: &str = "text/plain; charset=utf-8";

/// Text generation is usually fast, but reasoning models can be slow — keep the
/// same generous ceiling the image adapter uses.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) struct OpenAiChatAdapter {
    http: reqwest::Client,
}

impl OpenAiChatAdapter {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl MediaProvider for OpenAiChatAdapter {
    fn id(&self) -> &'static str {
        "openai_chat"
    }

    fn supports(&self, cap: MediaCapability) -> bool {
        matches!(cap, MediaCapability::Text)
    }

    async fn submit(&self, req: &SubmitRequest) -> Result<SubmitAck, CreationError> {
        let url = format!("{}/chat/completions", openai_versioned_base(&req.provider));
        let body = build_chat_body(&req.model, &req.params, &req.inputs);

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
        let value: Value = resp
            .json()
            .await
            .map_err(|e| CreationError::provider_error(format!("invalid chat JSON: {e}")))?;
        let text = parse_chat_response(&value)?;
        Ok(SubmitAck::Done(vec![text_asset(text)]))
    }

    async fn poll(&self, _remote: &str, _req: &SubmitRequest) -> Result<crate::provider::PollResult, CreationError> {
        Err(CreationError::config("openai_chat is synchronous and has no poll step"))
    }
}

/// Wrap generated text as a `text/plain` [`ProducedAsset`] (UTF-8 bytes). Shared
/// with the Gemini text adapter so both mint identical text artifacts.
pub(crate) fn text_asset(text: String) -> ProducedAsset {
    ProducedAsset { data: ProducedData::Bytes(text.into_bytes()), mime: Some(TEXT_MIME.to_string()) }
}

/// Build the `chat/completions` request body. Pure — unit tested.
///
/// - `params.system` (non-empty string) → a leading `system` message.
/// - The user message content is a plain string when there are no inputs, else
///   a multimodal array (prompt text + one segment per input).
/// - `params.max_tokens` (number) is forwarded only when present.
pub(crate) fn build_chat_body(model: &str, params: &Value, inputs: &[InputAsset]) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = param_system(params) {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": user_content(params, inputs)}));

    let mut body = json!({ "model": model, "messages": messages });
    if let Some(max) = params.get("max_tokens").and_then(|v| v.as_u64()) {
        body["max_tokens"] = json!(max);
    }
    body
}

/// The user-message `content`: a plain string when there are no inputs, else a
/// multimodal segment array (text prompt followed by image/text input segments).
fn user_content(params: &Value, inputs: &[InputAsset]) -> Value {
    let prompt = param_prompt(params);
    if inputs.is_empty() {
        return Value::String(prompt);
    }
    let mut segs: Vec<Value> = vec![json!({"type": "text", "text": prompt})];
    for input in inputs {
        segs.push(input_segment(input));
    }
    Value::Array(segs)
}

/// One content segment for an input asset: a text segment (decoded body) for a
/// `text/*` input, else an `image_url` data URL (image references and any other
/// binary reuse the vision channel).
fn input_segment(input: &InputAsset) -> Value {
    if input.mime.starts_with("text/") {
        json!({"type": "text", "text": String::from_utf8_lossy(&input.bytes)})
    } else {
        let data_url = format!("data:{};base64,{}", input.mime, encode_b64(&input.bytes));
        json!({"type": "image_url", "image_url": {"url": data_url}})
    }
}

/// The trimmed, non-empty `params.system` string, if any.
fn param_system(params: &Value) -> Option<String> {
    params
        .get("system")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract the assistant reply from a `chat/completions` body:
/// `choices[0].message.content`. Content may be a plain string or an array of
/// `{type:"text",text}` segments (concatenated). Pure — unit tested.
pub(crate) fn parse_chat_response(value: &Value) -> Result<String, CreationError> {
    let choices = value
        .get("choices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CreationError::provider_error("chat response missing 'choices' array"))?;
    let first = choices
        .first()
        .ok_or_else(|| CreationError::provider_error("chat response 'choices' is empty"))?;
    let content = first
        .get("message")
        .and_then(|m| m.get("content"))
        .ok_or_else(|| CreationError::provider_error("chat choice missing message.content"))?;

    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        return Err(CreationError::provider_error("chat response produced empty content"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_input(mime: &str, body: &str) -> InputAsset {
        InputAsset { asset_id: "wsa_x".into(), role: "reference".into(), bytes: body.as_bytes().to_vec(), mime: mime.into() }
    }

    #[test]
    fn parse_string_content() {
        let v = json!({"choices": [{"message": {"role": "assistant", "content": "hello world"}}]});
        assert_eq!(parse_chat_response(&v).unwrap(), "hello world");
    }

    #[test]
    fn parse_array_content_concatenates() {
        let v = json!({"choices": [{"message": {"content": [
            {"type": "text", "text": "foo "},
            {"type": "text", "text": "bar"}
        ]}}]});
        assert_eq!(parse_chat_response(&v).unwrap(), "foo bar");
    }

    #[test]
    fn parse_errors_on_missing_or_empty() {
        assert!(parse_chat_response(&json!({})).is_err());
        assert!(parse_chat_response(&json!({"choices": []})).is_err());
        assert!(parse_chat_response(&json!({"choices": [{}]})).is_err());
        assert!(parse_chat_response(&json!({"choices": [{"message": {"content": ""}}]})).is_err());
        assert!(parse_chat_response(&json!({"choices": [{"message": {"content": "   "}}]})).is_err());
    }

    #[test]
    fn body_plain_string_when_no_inputs() {
        let body = build_chat_body("gpt-4o", &json!({"prompt": "say hi"}), &[]);
        assert_eq!(body["model"], "gpt-4o");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "say hi");
        // max_tokens omitted by default
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn body_prepends_system_and_forwards_max_tokens() {
        let body = build_chat_body("m", &json!({"prompt": "hi", "system": "  be terse ", "max_tokens": 128}), &[]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be terse"); // trimmed
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(body["max_tokens"], 128);
    }

    #[test]
    fn body_blank_system_is_ignored() {
        let body = build_chat_body("m", &json!({"prompt": "hi", "system": "   "}), &[]);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn body_multimodal_with_image_and_text_inputs() {
        let inputs = vec![
            InputAsset { asset_id: "a".into(), role: "reference".into(), bytes: b"hi".to_vec(), mime: "image/png".into() },
            text_input("text/plain", "extra context"),
        ];
        let body = build_chat_body("gpt-4o", &json!({"prompt": "describe"}), &inputs);
        let content = &body["messages"][0]["content"];
        let segs = content.as_array().unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0]["type"], "text");
        assert_eq!(segs[0]["text"], "describe");
        // image → data URL image_url segment ("aGk=" is base64("hi"))
        assert_eq!(segs[1]["type"], "image_url");
        assert_eq!(segs[1]["image_url"]["url"], "data:image/png;base64,aGk=");
        // text input → text segment carrying the decoded body
        assert_eq!(segs[2]["type"], "text");
        assert_eq!(segs[2]["text"], "extra context");
    }

    #[test]
    fn text_asset_is_utf8_text_plain() {
        let a = text_asset("héllo".to_string());
        assert_eq!(a.mime.as_deref(), Some(TEXT_MIME));
        match a.data {
            ProducedData::Bytes(b) => assert_eq!(String::from_utf8(b).unwrap(), "héllo"),
            _ => panic!("expected bytes"),
        }
    }
}
