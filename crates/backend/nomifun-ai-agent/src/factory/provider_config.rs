//! Shared helper that resolves a Provider DB row into a fully-configured
//! `nomi_config::config::Config`, and a standalone one-shot LLM completion
//! function for the IDMM sidecar.

use std::path::Path;
use std::sync::Arc;

use nomi_config::config::{CliArgs, Config};
use nomi_providers::{LlmProvider, ProviderError, create_provider};
use nomi_types::llm::{LlmEvent, LlmRequest};
use nomi_types::message::{ContentBlock, Message, Role};
use nomifun_common::AppError;
use nomifun_db::IProviderRepository;

use crate::types::NomiCompatOverrides;

use super::nomi::{map_nomi_provider, resolve_bedrock_config, resolve_nomi_url_and_compat};

/// 依 registry 决定该 provider+model 的图片支持 override。
/// `Some(false)` = 已知不支持(发送时剔图);`None` = 未知(默认支持,行为不变)。
///
/// 只读进程级 `VisionUnsupportedRegistry`(内存,不落库),registry 无条目时
/// 返回 `None` → 下游 `compat.supports_image` 保持默认 `true`,现有行为不变。
pub(crate) fn image_support_override(provider_id: &str, model: &str) -> Option<bool> {
    if nomifun_common::VisionUnsupportedRegistry::global().is_unsupported(provider_id, model) {
        Some(false)
    } else {
        None
    }
}

/// Intermediate result of resolving a provider DB row before building a full
/// `Config`. Used internally by both `resolve_provider_config` and the nomi
/// agent factory to avoid duplicating the load+decrypt+map+url logic.
pub(crate) struct ResolvedProviderFields {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub compat_overrides: NomiCompatOverrides,
    pub bedrock_config: Option<nomi_config::config::BedrockConfig>,
    pub context_limit: Option<i64>,
}

/// Load a provider row from the DB, decrypt its API key, map platform to nomi
/// provider name, and resolve base URL / compat / bedrock fields.
///
/// This is the shared extraction used by both the full `resolve_provider_config`
/// (which also calls `Config::resolve`) and the nomi factory `build()` (which
/// passes the pieces into `NomiResolvedConfig`).
pub(crate) async fn resolve_provider_fields(
    provider_repo: &Arc<dyn IProviderRepository>,
    encryption_key: &[u8; 32],
    provider_id: &str,
    model: &str,
) -> Result<ResolvedProviderFields, AppError> {
    let row = provider_repo
        .find_by_id(provider_id)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to load provider config: {e}")))?
        .ok_or_else(|| AppError::BadRequest(format!("Provider '{provider_id}' not found")))?;

    let api_key = nomifun_common::decrypt_string(&row.api_key_encrypted, encryption_key)?;

    let provider = map_nomi_provider(&row.platform, model, row.model_protocols.as_deref());

    let (base_url, mut compat_overrides) =
        resolve_nomi_url_and_compat(&row.platform, &row.base_url, &provider, row.is_full_url);
    // 依进程级 registry 命中把「不支持图片」透传为 compat override(主动剔除)。
    // 未命中 → None → 下游默认 supports_image=true,现有行为不变。
    compat_overrides.supports_image = image_support_override(provider_id, model);

    let bedrock_config = if row.platform == "bedrock" {
        resolve_bedrock_config(row.bedrock_config.as_deref())
    } else {
        None
    };

    Ok(ResolvedProviderFields {
        provider,
        api_key,
        model: model.to_owned(),
        base_url,
        compat_overrides,
        bedrock_config,
        context_limit: row.context_limit,
    })
}

/// Resolve a provider DB row into a base `Config` suitable for LLM calls.
///
/// This performs: load provider row, decrypt API key, map platform to nomi
/// provider name, resolve base URL / compat overrides, build `CliArgs`,
/// call `Config::resolve`, then apply bedrock and compat post-assignments.
///
/// The returned `Config` does NOT include session-specific settings (MCP
/// servers, session directory, session mode) — callers layer those on top.
pub async fn resolve_provider_config(
    provider_repo: &Arc<dyn IProviderRepository>,
    encryption_key: &[u8; 32],
    provider_id: &str,
    model: &str,
    workspace: &Path,
) -> Result<Config, AppError> {
    let fields = resolve_provider_fields(provider_repo, encryption_key, provider_id, model).await?;

    let cli_args = CliArgs {
        provider: Some(fields.provider),
        api_key: Some(fields.api_key),
        base_url: fields.base_url,
        model: Some(fields.model),
        max_tokens: None,
        max_turns: None,
        system_prompt: None,
        profile: None,
        auto_approve: false,
        project_dir: Some(workspace.to_path_buf()),
    };

    let mut config =
        Config::resolve(&cli_args).map_err(|e| AppError::Internal(format!("Config resolve failed: {e}")))?;

    // Apply bedrock and compat post-assignments
    config.bedrock = fields.bedrock_config;

    if let Some(field) = fields.compat_overrides.max_tokens_field {
        config.compat.max_tokens_field = Some(field);
    }
    if let Some(path) = fields.compat_overrides.api_path {
        config.compat.api_path = Some(path);
    }
    // NB: compat_overrides.supports_image is intentionally NOT applied here —
    // this one-shot path (IDMM sidecar) builds text-only messages, so image
    // stripping is moot. Only the nomi agent manager applies it. Do not add it
    // for "consistency"; it would be dead config on this path.

    Ok(config)
}

/// Which stream channel a delta came from, so callers can route reasoning
/// (thinking) deltas separately from the visible text answer.
///
/// Used by [`streaming_completion_kinded`]: `Text` = `LlmEvent::TextDelta`
/// (the visible answer — what the final assembled string is built from);
/// `Reasoning` = `LlmEvent::ThinkingDelta` (the model's readable reasoning,
/// fanned out for observability but NOT part of the returned text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// A `TextDelta` — the visible answer text (assembled into the return value).
    Text,
    /// A `ThinkingDelta` — the model's reasoning (forwarded, not assembled).
    Reasoning,
}

/// Perform a single-turn LLM completion and return the assembled text response.
///
/// Builds an `LlmRequest` from the given config, streams events from the
/// provider, and concatenates `TextDelta` events until `Done` is received.
/// Errors from the provider or the stream are mapped to `AppError::BadGateway`.
pub async fn one_shot_completion(
    cfg: &Config,
    system: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, AppError> {
    streaming_completion(cfg, system, messages, max_tokens, |_| {}).await
}

/// Like [`one_shot_completion`] but invokes `on_delta` for every text chunk
/// as it streams in, so callers can fan deltas out (e.g. over WebSocket)
/// while the full reply is still being assembled.
pub async fn streaming_completion(
    cfg: &Config,
    system: &str,
    messages: Vec<Message>,
    max_tokens: u32,
    on_delta: impl FnMut(&str) + Send,
) -> Result<String, AppError> {
    let provider: Arc<dyn LlmProvider> = create_provider(cfg);

    let request = LlmRequest {
        model: cfg.model.clone(),
        system: system.to_owned(),
        messages,
        tools: vec![],
        max_tokens,
        thinking: None,
        reasoning_effort: None,
    };

    let rx = provider.stream(&request).await.map_err(provider_error_to_app_error)?;

    drain_text_response_with(rx, on_delta).await
}

/// Like [`streaming_completion`] but the `on_delta` callback ALSO receives a
/// [`DeltaKind`] so a caller can fan out the model's reasoning (`ThinkingDelta`)
/// separately from the visible answer (`TextDelta`).
///
/// The returned String is built from `TextDelta` events ONLY — exactly the same
/// bytes [`streaming_completion`] / [`one_shot_completion`] return. `ThinkingDelta`
/// events are forwarded to `on_delta` with [`DeltaKind::Reasoning`] but never
/// appended to the result, so the assembled text stays identical to the one-shot
/// path (a caller that ignores the reasoning gets byte-for-byte the same answer).
pub async fn streaming_completion_kinded(
    cfg: &Config,
    system: &str,
    messages: Vec<Message>,
    max_tokens: u32,
    on_delta: impl FnMut(DeltaKind, &str) + Send,
) -> Result<String, AppError> {
    let provider: Arc<dyn LlmProvider> = create_provider(cfg);

    let request = LlmRequest {
        model: cfg.model.clone(),
        system: system.to_owned(),
        messages,
        tools: vec![],
        max_tokens,
        thinking: None,
        reasoning_effort: None,
    };

    let rx = provider.stream(&request).await.map_err(provider_error_to_app_error)?;

    drain_text_response_kinded(rx, on_delta).await
}

/// Like [`streaming_completion_kinded`] but for the ORCHESTRATION PLANNER: when the
/// model emits its answer ONLY in the reasoning channel (empty `content`), the
/// returned String falls back to the assembled reasoning text (see
/// [`drain_text_or_reasoning`]) so a requested JSON can still be recovered. Both
/// `Text` and `Reasoning` deltas are forwarded to `on_delta` (for lead-thinking
/// fan-out). This DIVERGES from the visible-answer one-shot semantics on purpose and
/// must only be used where reasoning-as-answer is acceptable (planning / re-plan).
pub async fn streaming_completion_text_or_reasoning(
    cfg: &Config,
    system: &str,
    messages: Vec<Message>,
    max_tokens: u32,
    on_delta: impl FnMut(DeltaKind, &str) + Send,
) -> Result<String, AppError> {
    let provider: Arc<dyn LlmProvider> = create_provider(cfg);

    let request = LlmRequest {
        model: cfg.model.clone(),
        system: system.to_owned(),
        messages,
        tools: vec![],
        max_tokens,
        thinking: None,
        reasoning_effort: None,
    };

    let rx = provider.stream(&request).await.map_err(provider_error_to_app_error)?;

    drain_text_or_reasoning(rx, on_delta).await
}

/// Convenience constructor for a user-role `Message` with a single text block.
pub fn user_message(text: impl Into<String>) -> Message {
    Message::new(Role::User, vec![ContentBlock::Text { text: text.into() }])
}

/// Drain an `LlmEvent` receiver, concatenating `TextDelta` payloads until
/// `Done` is received. Returns the assembled text or an error.
#[cfg(test)]
async fn drain_text_response(rx: tokio::sync::mpsc::Receiver<LlmEvent>) -> Result<String, AppError> {
    drain_text_response_with(rx, |_| {}).await
}

/// Drain variant that surfaces every text delta to `on_delta` as it arrives.
async fn drain_text_response_with(
    mut rx: tokio::sync::mpsc::Receiver<LlmEvent>,
    mut on_delta: impl FnMut(&str) + Send,
) -> Result<String, AppError> {
    let mut output = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::TextDelta(delta) => {
                on_delta(&delta);
                output.push_str(&delta);
            }
            LlmEvent::Done { .. } => return Ok(output),
            LlmEvent::Error(msg) => {
                return Err(AppError::BadGateway(format!("LLM stream error: {msg}")));
            }
            // Ignore thinking deltas, tool use, and signatures for one-shot
            _ => {}
        }
    }

    // Channel closed without a Done event
    if output.is_empty() {
        Err(AppError::BadGateway(
            "LLM stream ended without producing a response".into(),
        ))
    } else {
        Ok(output)
    }
}

/// Drain variant that surfaces BOTH text and thinking deltas to `on_delta`,
/// tagged with their [`DeltaKind`]. Only `TextDelta` payloads are appended to the
/// returned String (so it equals the one-shot answer); `ThinkingDelta` payloads
/// are forwarded as [`DeltaKind::Reasoning`] for observability but never assembled.
async fn drain_text_response_kinded(
    mut rx: tokio::sync::mpsc::Receiver<LlmEvent>,
    mut on_delta: impl FnMut(DeltaKind, &str) + Send,
) -> Result<String, AppError> {
    let mut output = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::TextDelta(delta) => {
                on_delta(DeltaKind::Text, &delta);
                output.push_str(&delta);
            }
            LlmEvent::ThinkingDelta(delta) => {
                // Forward reasoning for fan-out, but DO NOT append it — the
                // returned text must stay identical to the one-shot answer.
                on_delta(DeltaKind::Reasoning, &delta);
            }
            LlmEvent::Done { .. } => return Ok(output),
            LlmEvent::Error(msg) => {
                return Err(AppError::BadGateway(format!("LLM stream error: {msg}")));
            }
            // Ignore tool use and thinking signatures.
            _ => {}
        }
    }

    // Channel closed without a Done event
    if output.is_empty() {
        Err(AppError::BadGateway(
            "LLM stream ended without producing a response".into(),
        ))
    } else {
        Ok(output)
    }
}

/// Drain variant for the ORCHESTRATION PLANNER: assembles `TextDelta` into the
/// primary buffer AND `ThinkingDelta` into a separate reasoning buffer (forwarding
/// both to `on_delta`, tagged). On `Done`, returns the text buffer when it has
/// content, otherwise FALLS BACK to the reasoning buffer.
///
/// Some OpenAI-compatible reasoning models (e.g. StepFun `step-*`) put their entire
/// answer — including a requested JSON — in the `reasoning_content` channel
/// (→ `ThinkingDelta`) and leave `content` (→ `TextDelta`) empty. The standard drain
/// then returns `Ok("")` on `Done`, so the planner saw an empty completion and fell
/// back to a single-task DAG (会话10). Returning the reasoning text on empty content
/// lets `extract_json_object` still recover the plan JSON. Kept separate from
/// [`drain_text_response_kinded`] so the visible-answer one-shot semantics (and its
/// reasoning-only→Err contract) are unchanged.
async fn drain_text_or_reasoning(
    mut rx: tokio::sync::mpsc::Receiver<LlmEvent>,
    mut on_delta: impl FnMut(DeltaKind, &str) + Send,
) -> Result<String, AppError> {
    let mut text = String::new();
    let mut reasoning = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::TextDelta(delta) => {
                on_delta(DeltaKind::Text, &delta);
                text.push_str(&delta);
            }
            LlmEvent::ThinkingDelta(delta) => {
                on_delta(DeltaKind::Reasoning, &delta);
                reasoning.push_str(&delta);
            }
            LlmEvent::Done { .. } => {
                return Ok(if text.trim().is_empty() { reasoning } else { text });
            }
            LlmEvent::Error(msg) => {
                return Err(AppError::BadGateway(format!("LLM stream error: {msg}")));
            }
            // Ignore tool use and thinking signatures.
            _ => {}
        }
    }

    // Channel closed without a Done event: prefer text, else reasoning.
    let out = if text.trim().is_empty() { reasoning } else { text };
    if out.is_empty() {
        Err(AppError::BadGateway(
            "LLM stream ended without producing a response".into(),
        ))
    } else {
        Ok(out)
    }
}

fn provider_error_to_app_error(e: ProviderError) -> AppError {
    AppError::BadGateway(format!("LLM provider error: {e}"))
}

#[cfg(test)]
mod image_override_tests {
    use super::*;

    #[test]
    fn override_none_when_not_marked() {
        assert_eq!(image_support_override("unlikely-prov-xyz", "unlikely-model"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_creates_correct_structure() {
        let msg = user_message("Hello, world!");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello, world!"),
            _ => panic!("Expected Text content block"),
        }
        assert!(msg.timestamp.is_none());
    }

    #[test]
    fn user_message_accepts_string() {
        let owned = String::from("test input");
        let msg = user_message(owned);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "test input"),
            _ => panic!("Expected Text content block"),
        }
    }

    #[tokio::test]
    async fn drain_text_response_concatenates_deltas() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(LlmEvent::TextDelta("Hello".into())).await.unwrap();
        tx.send(LlmEvent::TextDelta(", world!".into())).await.unwrap();
        tx.send(LlmEvent::Done {
            stop_reason: nomi_types::message::StopReason::EndTurn,
            usage: nomi_types::message::TokenUsage::default(),
        })
        .await
        .unwrap();

        let result = drain_text_response(rx).await.unwrap();
        assert_eq!(result, "Hello, world!");
    }

    #[tokio::test]
    async fn drain_text_response_returns_error_on_llm_error_event() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(LlmEvent::TextDelta("partial".into())).await.unwrap();
        tx.send(LlmEvent::Error("rate limited".into())).await.unwrap();

        let result = drain_text_response(rx).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AppError::BadGateway(_)));
    }

    #[tokio::test]
    async fn drain_text_response_returns_partial_on_channel_close() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(LlmEvent::TextDelta("partial output".into())).await.unwrap();
        drop(tx); // close channel without Done

        let result = drain_text_response(rx).await.unwrap();
        assert_eq!(result, "partial output");
    }

    #[tokio::test]
    async fn drain_text_response_errors_on_empty_channel_close() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<LlmEvent>(8);
        drop(_tx);

        let result = drain_text_response(rx).await;
        assert!(result.is_err());
    }

    #[test]
    fn provider_error_maps_to_bad_gateway() {
        let err = provider_error_to_app_error(ProviderError::Connection("timeout".into()));
        assert!(matches!(err, AppError::BadGateway(_)));
    }

    // The kinded drain forwards TextDelta as DeltaKind::Text and ThinkingDelta as
    // DeltaKind::Reasoning, but the RETURNED text is built from TextDelta ONLY —
    // byte-for-byte identical to the one-shot answer (reasoning is observability,
    // not part of the answer).
    #[tokio::test]
    async fn drain_kinded_routes_by_kind_and_text_equals_one_shot() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(LlmEvent::ThinkingDelta("let me ".into())).await.unwrap();
        tx.send(LlmEvent::TextDelta("Hello".into())).await.unwrap();
        tx.send(LlmEvent::ThinkingDelta("think…".into())).await.unwrap();
        tx.send(LlmEvent::TextDelta(", world!".into())).await.unwrap();
        tx.send(LlmEvent::Done {
            stop_reason: nomi_types::message::StopReason::EndTurn,
            usage: nomi_types::message::TokenUsage::default(),
        })
        .await
        .unwrap();

        let mut text_chunks: Vec<String> = Vec::new();
        let mut reasoning_chunks: Vec<String> = Vec::new();
        let result = drain_text_response_kinded(rx, |kind, delta| match kind {
            DeltaKind::Text => text_chunks.push(delta.to_string()),
            DeltaKind::Reasoning => reasoning_chunks.push(delta.to_string()),
        })
        .await
        .unwrap();

        // Reasoning deltas were forwarded separately…
        assert_eq!(reasoning_chunks, vec!["let me ", "think…"]);
        // …text deltas were forwarded as Text…
        assert_eq!(text_chunks, vec!["Hello", ", world!"]);
        // …and the assembled answer == ONLY the TextDelta concat (one-shot equiv).
        assert_eq!(result, "Hello, world!");
    }

    // 会话10 fix: a reasoning-only stream (Done with EMPTY content) — as StepFun
    // `step-*` produces — must return the REASONING text, not "", so the planner can
    // still recover the JSON. The standard `drain_text_response_kinded` returns ""
    // here; `drain_text_or_reasoning` falls back to reasoning.
    #[tokio::test]
    async fn drain_text_or_reasoning_falls_back_to_reasoning_on_empty_text() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(LlmEvent::ThinkingDelta(r#"{"tasks":[{"title":"A",""#.into())).await.unwrap();
        tx.send(LlmEvent::ThinkingDelta(r#"spec":"a","depends_on":[]}]}"#.into())).await.unwrap();
        tx.send(LlmEvent::Done {
            stop_reason: nomi_types::message::StopReason::EndTurn,
            usage: nomi_types::message::TokenUsage::default(),
        })
        .await
        .unwrap();

        let result = drain_text_or_reasoning(rx, |_, _| {}).await.unwrap();
        assert_eq!(result, r#"{"tasks":[{"title":"A","spec":"a","depends_on":[]}]}"#);
    }

    // When BOTH text and reasoning are present, the text (visible answer) wins — the
    // reasoning fallback only kicks in for an EMPTY text buffer.
    #[tokio::test]
    async fn drain_text_or_reasoning_prefers_text_when_present() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(LlmEvent::ThinkingDelta("thinking…".into())).await.unwrap();
        tx.send(LlmEvent::TextDelta("real answer".into())).await.unwrap();
        tx.send(LlmEvent::Done {
            stop_reason: nomi_types::message::StopReason::EndTurn,
            usage: nomi_types::message::TokenUsage::default(),
        })
        .await
        .unwrap();

        let result = drain_text_or_reasoning(rx, |_, _| {}).await.unwrap();
        assert_eq!(result, "real answer");
    }

    // The kinded drain surfaces a stream Error as BadGateway, exactly like the
    // text-only drain.
    #[tokio::test]
    async fn drain_kinded_errors_on_llm_error_event() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(LlmEvent::TextDelta("partial".into())).await.unwrap();
        tx.send(LlmEvent::Error("rate limited".into())).await.unwrap();

        let result = drain_text_response_kinded(rx, |_, _| {}).await;
        assert!(matches!(result.unwrap_err(), AppError::BadGateway(_)));
    }

    // A reasoning-only stream (no TextDelta) still ends without a visible answer →
    // the empty-channel-close contract (BadGateway), matching one_shot semantics
    // (thinking alone is not an answer).
    #[tokio::test]
    async fn drain_kinded_reasoning_only_close_errors_empty() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(LlmEvent::ThinkingDelta("only thinking".into())).await.unwrap();
        drop(tx); // close without Done and without any TextDelta

        let mut saw_reasoning = false;
        let result = drain_text_response_kinded(rx, |kind, _| {
            if kind == DeltaKind::Reasoning {
                saw_reasoning = true;
            }
        })
        .await;
        assert!(saw_reasoning, "reasoning still forwarded");
        assert!(result.is_err(), "no TextDelta → no answer → error on empty close");
    }
}
