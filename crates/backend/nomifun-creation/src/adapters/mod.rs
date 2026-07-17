//! Media provider adapters + the capability→adapter routing.
//!
//! Each adapter is a [`crate::provider::MediaProvider`] over a concrete remote
//! protocol:
//! - [`openai_images`] — OpenAI-compatible sync `/v1/images/{generations,edits}`
//!   (t2i / i2i / inpaint); artifacts return inline.
//! - [`gemini_image`] — Google `:generateContent` with image response modality
//!   (t2i / i2i).
//! - [`openai_video`] — OpenAI-compatible async `/v1/videos` submit→poll→content
//!   (t2v / i2v).
//! - [`openai_chat`] — OpenAI-compatible sync `/v1/chat/completions` (`text`);
//!   the reply is returned inline as `text/plain` UTF-8.
//! - [`gemini_text`] — Google `:generateContent` text mode (`text`).
//!
//! `ark` (火山方舟) and `modelscope` are P1 stubs (empty module files that keep
//! the extension seam explicit). Register the live adapters on the
//! [`crate::CreationService`] at app-assembly time via [`default_adapters`].

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::provider::{MediaProvider, ResolvedProvider};
use crate::types::MediaCapability;

pub(crate) mod ark;
pub(crate) mod gemini_image;
pub(crate) mod gemini_text;
pub(crate) mod modelscope;
pub(crate) mod openai_chat;
pub(crate) mod openai_images;
pub(crate) mod openai_video;

/// Build the standard adapter set (P0/P1 live protocols) over a shared,
/// proxy-aware HTTP client. Registered on the service at assembly time.
pub fn default_adapters(http: reqwest::Client) -> Vec<Arc<dyn MediaProvider>> {
    vec![
        Arc::new(openai_images::OpenAiImagesAdapter::new(http.clone())),
        Arc::new(gemini_image::GeminiImageAdapter::new(http.clone())),
        Arc::new(openai_video::OpenAiVideoAdapter::new(http.clone())),
        Arc::new(openai_chat::OpenAiChatAdapter::new(http.clone())),
        Arc::new(gemini_text::GeminiTextAdapter::new(http)),
    ]
}

/// Pick the adapter id for a `(capability, platform, model)` triple. Explicit,
/// extensible dispatch (P1 adds `ark` / `modelscope` branches):
/// - video caps → `openai_video`;
/// - `gemini` platform or a model whose name contains `gemini` → `gemini_image`
///   (which serves t2i/i2i; inpaint always falls to `openai_images`);
/// - everything else image → `openai_images`;
/// - `text` → `gemini_text` (gemini platform/model) or `openai_chat`.
///
/// Returns `None` for capabilities no adapter routes yet (`tts`).
pub fn route_adapter_id(cap: MediaCapability, platform: &str, model: &str) -> Option<&'static str> {
    use MediaCapability::*;
    match cap {
        T2v | I2v | V2v => Some("openai_video"),
        Inpaint => Some("openai_images"),
        T2i | I2i => {
            if is_gemini(platform, model) {
                Some("gemini_image")
            } else {
                Some("openai_images")
            }
        }
        Text => {
            if is_gemini(platform, model) {
                Some("gemini_text")
            } else {
                Some("openai_chat")
            }
        }
        Tts => None,
    }
}

fn is_gemini(platform: &str, model: &str) -> bool {
    platform.eq_ignore_ascii_case("gemini") || model.to_ascii_lowercase().contains("gemini")
}

// ---------------------------------------------------------------------------
// Endpoint composition (is_full_url semantics mirror nomifun-ai-agent's
// `resolve_nomi_url_and_compat`: when `is_full_url` the base already carries
// the version path, so we append only the operation-relative path).
// ---------------------------------------------------------------------------

/// The OpenAI-compatible version root a `/images/*` or `/videos` path is
/// appended to. Non-full-url bases get a single `/v1` normalized on; full-url
/// bases are used verbatim.
pub(crate) fn openai_versioned_base(p: &ResolvedProvider) -> String {
    let b = p.base_url.trim_end_matches('/');
    if p.is_full_url {
        b.to_string()
    } else {
        let root = b.strip_suffix("/v1").unwrap_or(b);
        format!("{root}/v1")
    }
}

/// The Gemini `:generateContent` URL for a model. Gemini uses a `/v1beta/models`
/// scheme rather than `/v1`; a trailing `/v1beta` on the configured base is
/// tolerated (stripped then re-added) so both `https://host` and
/// `https://host/v1beta` resolve identically.
pub(crate) fn gemini_generate_url(p: &ResolvedProvider, model: &str) -> String {
    let b = p.base_url.trim_end_matches('/');
    let root = b.strip_suffix("/v1beta").unwrap_or(b);
    format!("{root}/v1beta/models/{model}:generateContent")
}

// ---------------------------------------------------------------------------
// Shared param + response helpers used across adapters.
// ---------------------------------------------------------------------------

/// The prompt string from the opaque params (`""` when absent).
pub(crate) fn param_prompt(params: &serde_json::Value) -> String {
    params.get("prompt").and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// Batch count (`params.count`), clamped to 1..=10; defaults to 1.
pub(crate) fn param_count(params: &serde_json::Value) -> u32 {
    params.get("count").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 10) as u32
}

/// A `WxH` size string from `params.width`/`params.height`, or an explicit
/// `params.size` string, else `None`.
pub(crate) fn param_size(params: &serde_json::Value) -> Option<String> {
    let w = params.get("width").and_then(|v| v.as_u64());
    let h = params.get("height").and_then(|v| v.as_u64());
    if let (Some(w), Some(h)) = (w, h) {
        return Some(format!("{w}x{h}"));
    }
    params
        .get("size")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Decode a base64 image payload (adapters share this for inline results).
pub(crate) fn decode_b64(s: &str) -> Option<Vec<u8>> {
    BASE64.decode(s.trim()).ok()
}

/// Encode input bytes to base64 (Gemini inline_data).
pub(crate) fn encode_b64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

// ---------------------------------------------------------------------------
// Shared HTTP error mapping.
// ---------------------------------------------------------------------------

/// Map a reqwest transport error to a [`CreationError`] (timeout vs generic).
pub(crate) fn net_err(e: reqwest::Error) -> crate::types::CreationError {
    if e.is_timeout() {
        crate::types::CreationError::timeout(format!("request timed out: {e}"))
    } else {
        crate::types::CreationError::provider_error(format!("request failed: {e}"))
    }
}

/// Build a `provider_error` from a non-2xx response, folding the status +
/// (truncated) body into the message.
pub(crate) async fn error_from_response(resp: reqwest::Response) -> crate::types::CreationError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(500).collect();
    crate::types::CreationError::provider_error(format!("provider returned {status}: {snippet}"))
        .with_http_status(status.as_u16())
}

/// Hard ceiling on a single downloaded artifact / video-content body. Streams
/// are aborted once this is exceeded so a large or hostile provider response
/// cannot exhaust process memory.
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Read a response body fully into memory under a hard byte cap. Rejects early
/// on an oversized `Content-Length`, then streams chunk-by-chunk (Content-Length
/// may be absent or spoofed) aborting the moment the running total would exceed
/// `max_bytes`. Replaces the unbounded `resp.bytes()` used for artifact/video
/// downloads.
pub(crate) async fn read_body_capped(
    mut resp: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, crate::types::CreationError> {
    use crate::types::CreationError;
    if let Some(len) = resp.content_length()
        && len > max_bytes
    {
        return Err(CreationError::provider_error(format!(
            "artifact too large: declared {len} bytes exceeds cap of {max_bytes}"
        )));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(net_err)? {
        if buf.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(CreationError::provider_error(format!(
                "artifact exceeded size cap of {max_bytes} bytes"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base: &str, is_full_url: bool) -> ResolvedProvider {
        ResolvedProvider {
            provider_id: nomifun_common::ProviderId::new().into_string(),
            platform: "openai".into(),
            base_url: base.into(),
            api_key: "sk".into(),
            is_full_url,
        }
    }

    #[test]
    fn route_picks_expected_adapters() {
        use MediaCapability::*;
        assert_eq!(route_adapter_id(T2i, "openai", "gpt-image-1"), Some("openai_images"));
        assert_eq!(route_adapter_id(I2i, "openai", "gpt-image-1"), Some("openai_images"));
        assert_eq!(route_adapter_id(Inpaint, "openai", "gpt-image-1"), Some("openai_images"));
        // gemini by platform OR by model-name substring
        assert_eq!(route_adapter_id(T2i, "gemini", "nano"), Some("gemini_image"));
        assert_eq!(route_adapter_id(T2i, "custom", "gemini-2.5-flash-image"), Some("gemini_image"));
        // inpaint never routes to gemini (it can't serve it)
        assert_eq!(route_adapter_id(Inpaint, "gemini", "gemini-x"), Some("openai_images"));
        // video
        assert_eq!(route_adapter_id(T2v, "openai", "sora-2"), Some("openai_video"));
        assert_eq!(route_adapter_id(I2v, "openai", "sora-2"), Some("openai_video"));
        // text → openai_chat by default, gemini_text by platform OR model substring
        assert_eq!(route_adapter_id(Text, "openai", "gpt-4o"), Some("openai_chat"));
        assert_eq!(route_adapter_id(Text, "gemini", "gemini-2.5-pro"), Some("gemini_text"));
        assert_eq!(route_adapter_id(Text, "custom", "gemini-flash"), Some("gemini_text"));
        // unrouted
        assert_eq!(route_adapter_id(Tts, "openai", "tts-1"), None);
    }

    #[test]
    fn openai_versioned_base_normalizes() {
        assert_eq!(openai_versioned_base(&provider("https://api.openai.com/v1", false)), "https://api.openai.com/v1");
        assert_eq!(openai_versioned_base(&provider("https://api.openai.com", false)), "https://api.openai.com/v1");
        assert_eq!(openai_versioned_base(&provider("https://api.openai.com/", false)), "https://api.openai.com/v1");
        // full-url base is used verbatim (no extra /v1)
        assert_eq!(openai_versioned_base(&provider("https://proxy/openai/v1", true)), "https://proxy/openai/v1");
    }

    #[test]
    fn gemini_url_composes() {
        assert_eq!(
            gemini_generate_url(&provider("https://generativelanguage.googleapis.com", false), "gemini-2.5-flash-image"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent"
        );
        // trailing /v1beta tolerated
        assert_eq!(
            gemini_generate_url(&provider("https://host/v1beta", false), "m"),
            "https://host/v1beta/models/m:generateContent"
        );
    }

    #[test]
    fn param_helpers() {
        let p = serde_json::json!({"prompt": "a cat", "width": 512, "height": 768, "count": 3});
        assert_eq!(param_prompt(&p), "a cat");
        assert_eq!(param_count(&p), 3);
        assert_eq!(param_size(&p).as_deref(), Some("512x768"));

        let p2 = serde_json::json!({"size": "1024x1024", "count": 99});
        assert_eq!(param_size(&p2).as_deref(), Some("1024x1024"));
        assert_eq!(param_count(&p2), 10); // clamped
        assert_eq!(param_count(&serde_json::json!({})), 1); // default
        assert_eq!(param_prompt(&serde_json::json!({})), "");
        assert!(param_size(&serde_json::json!({})).is_none());
    }

    #[tokio::test]
    async fn read_body_capped_enforces_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/artifact"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 100]))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/artifact", server.uri());

        // Over the cap → error (rejected on the declared Content-Length).
        let resp = client.get(&url).send().await.unwrap();
        assert!(read_body_capped(resp, 10).await.is_err(), "oversized body must be rejected");

        // Within the cap → full body returned (streaming accumulation path).
        let resp2 = client.get(&url).send().await.unwrap();
        let body = read_body_capped(resp2, 1024).await.unwrap();
        assert_eq!(body.len(), 100);
    }
}
