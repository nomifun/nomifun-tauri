//! Autocompact: watermark-triggered LLM summarization.
//!
//! When the token watermark exceeds the configured threshold, this module
//! calls the LLM to produce a structured summary of the conversation,
//! then replaces the full history with a compact boundary marker and the
//! summary.  A circuit breaker prevents runaway retries.

use nomi_config::compact::CompactConfig;
use nomi_providers::{LlmProvider, ProviderError};
use nomi_types::compact::{CompactMetadata, CompactTrigger};
use nomi_types::llm::{LlmEvent, LlmRequest, ThinkingConfig};
use nomi_types::message::{ContentBlock, Message, Role, TokenUsage};
use tokio::sync::mpsc;

use super::prompt::{
    COMPACT_SYSTEM_PROMPT, build_compact_prompt, build_summary_content,
    compact_max_output_tokens, format_compact_summary,
};
use super::state::CompactState;

/// Maximum number of prompt-too-long retries.
const MAX_PTL_RETRIES: u32 = 2;

/// Content prefix for the compact boundary marker message.
pub const BOUNDARY_PREFIX: &str = "[Conversation compacted]";

// ── Public types ────────────────────────────────────────────────────────────

/// Result of a successful autocompact operation.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Post-compact messages that replace the original conversation.
    /// Contains a boundary marker and a summary message.
    pub messages: Vec<Message>,
    /// How many original messages were summarized.
    pub messages_summarized: usize,
    /// Input token count before compaction (from the last API call).
    pub pre_compact_tokens: u64,
}

/// Errors specific to autocompact.
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("LLM provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("Prompt too long after {attempts} retries")]
    PromptTooLong { attempts: u32 },
    #[error("Empty response from LLM")]
    EmptyResponse,
    #[error("Stream error: {0}")]
    StreamError(String),
    #[error("Circuit breaker tripped after {failures} consecutive failures")]
    CircuitBroken { failures: u32 },
}

// ── Trigger check ───────────────────────────────────────────────────────────

/// Check if autocompact should trigger based on the token watermark.
///
/// When `autocompact_threshold_pct` is set, threshold = context_window * pct / 100.
/// Otherwise falls back to: `threshold = context_window - output_reserve - autocompact_buffer`
pub fn should_autocompact(last_input_tokens: u64, config: &CompactConfig) -> bool {
    if !config.enabled {
        return false;
    }
    let threshold = if let Some(pct) = config.autocompact_threshold_pct {
        config.context_window * pct as usize / 100
    } else {
        let effective_window = config.context_window.saturating_sub(config.output_reserve);
        effective_window.saturating_sub(config.autocompact_buffer)
    };
    last_input_tokens as usize >= threshold
}

// ── Core autocompact ────────────────────────────────────────────────────────

/// Execute autocompact: call LLM to summarize the conversation.
///
/// 1. Build a summary prompt and send conversation + prompt to the LLM.
/// 2. If the prompt is too long, truncate oldest 20% messages and retry
///    (up to [`MAX_PTL_RETRIES`] times).
/// 3. Parse the `<summary>` from the response.
/// 4. Return a [`CompactResult`] with boundary marker + summary messages.
///
/// On failure, increments `state.consecutive_failures`.
/// On success, resets the failure counter.
pub async fn autocompact(
    provider: &dyn LlmProvider,
    messages: &[Message],
    model: &str,
    config: &CompactConfig,
    state: &mut CompactState,
) -> Result<CompactResult, CompactError> {
    // Circuit breaker check
    if state.is_circuit_broken(config) {
        return Err(CompactError::CircuitBroken {
            failures: state.consecutive_failures,
        });
    }

    let pre_compact_tokens = state.last_input_tokens;
    let messages_summarized = messages.len();

    // Build messages for the compact LLM call: conversation + summary prompt
    let prompt = build_compact_prompt();
    let mut conv_messages = messages.to_vec();
    conv_messages.push(Message::new(
        Role::User,
        vec![ContentBlock::Text { text: prompt }],
    ));

    let mut ptl_attempts = 0u32;

    let summary_text = loop {
        let request = LlmRequest {
            model: model.to_string(),
            system: COMPACT_SYSTEM_PROMPT.to_string(),
            messages: conv_messages.clone(),
            tools: vec![],
            max_tokens: compact_max_output_tokens(config.context_window),
            thinking: Some(ThinkingConfig::Disabled),
            reasoning_effort: None,
        };

        match provider.stream(&request).await {
            Ok(rx) => match collect_stream_text(rx).await {
                Ok((text, _usage)) => break text,
                Err(e) => {
                    state.record_failure();
                    return Err(e);
                }
            },
            Err(ProviderError::PromptTooLong(_)) if ptl_attempts < MAX_PTL_RETRIES => {
                ptl_attempts += 1;
                // Remove the summary prompt (last msg), truncate, re-add prompt
                let conversation_part = &conv_messages[..conv_messages.len() - 1];
                match truncate_for_retry(conversation_part) {
                    Some(mut truncated) => {
                        truncated.push(Message::new(
                            Role::User,
                            vec![ContentBlock::Text {
                                text: build_compact_prompt(),
                            }],
                        ));
                        conv_messages = truncated;
                    }
                    None => {
                        state.record_failure();
                        return Err(CompactError::PromptTooLong {
                            attempts: ptl_attempts,
                        });
                    }
                }
            }
            Err(ProviderError::PromptTooLong(_)) => {
                state.record_failure();
                return Err(CompactError::PromptTooLong {
                    attempts: ptl_attempts,
                });
            }
            Err(e) => {
                state.record_failure();
                return Err(CompactError::Provider(e));
            }
        }
    };

    if summary_text.trim().is_empty() {
        state.record_failure();
        return Err(CompactError::EmptyResponse);
    }

    // Format and build post-compact messages
    let formatted = format_compact_summary(&summary_text);
    let summary_content = build_summary_content(&formatted, true);

    let metadata = CompactMetadata {
        trigger: CompactTrigger::Auto,
        pre_compact_tokens,
        messages_summarized,
    };

    let boundary_text = format!(
        "{BOUNDARY_PREFIX}\n{}",
        serde_json::to_string(&metadata).expect("CompactMetadata serialization cannot fail")
    );

    let boundary_msg = Message::new(
        Role::User,
        vec![ContentBlock::Text {
            text: boundary_text,
        }],
    );

    let summary_msg = Message::new(
        Role::User,
        vec![ContentBlock::Text {
            text: summary_content,
        }],
    );

    state.record_success();

    Ok(CompactResult {
        messages: vec![boundary_msg, summary_msg],
        messages_summarized,
        pre_compact_tokens,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Collect all text from a streaming LLM response.
async fn collect_stream_text(
    mut rx: mpsc::Receiver<LlmEvent>,
) -> Result<(String, TokenUsage), CompactError> {
    let mut text = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::Done { usage, .. } => return Ok((text, usage)),
            LlmEvent::Error(e) => return Err(CompactError::StreamError(e)),
            // Ignore thinking deltas and tool calls (shouldn't happen in compact)
            _ => {}
        }
    }

    // Channel closed without a Done event
    Err(CompactError::EmptyResponse)
}

/// Truncate the oldest ~20% of messages for PTL retry.
///
/// Returns `None` if there are too few messages to truncate meaningfully.
fn truncate_for_retry(messages: &[Message]) -> Option<Vec<Message>> {
    if messages.len() < 2 {
        return None;
    }

    let drop_count = (messages.len() / 5).max(1);
    if drop_count >= messages.len() {
        return None;
    }

    let remaining = &messages[drop_count..];
    let mut result = Vec::with_capacity(remaining.len() + 1);

    // Ensure the first message is User role for API compatibility
    if remaining.first().map(|m| m.role) != Some(Role::User) {
        result.push(Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "[earlier conversation truncated for compaction retry]".to_string(),
            }],
        ));
    }

    result.extend_from_slice(remaining);
    Some(result)
}

/// Check if a message is a compact boundary marker.
pub fn is_compact_boundary(message: &Message) -> bool {
    message.content.iter().any(|block| {
        if let ContentBlock::Text { text } = block {
            text.starts_with(BOUNDARY_PREFIX)
        } else {
            false
        }
    })
}

/// Extract [`CompactMetadata`] from a boundary marker message.
pub fn extract_compact_metadata(message: &Message) -> Option<CompactMetadata> {
    for block in &message.content {
        if let ContentBlock::Text { text } = block
            && let Some(json_str) = text.strip_prefix(BOUNDARY_PREFIX)
        {
            let json_str = json_str.trim_start_matches('\n');
            return serde_json::from_str(json_str).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_types::compact::CompactTrigger;

    fn default_config() -> CompactConfig {
        CompactConfig::default()
    }

    // ── should_autocompact (TC-2.4-01..03, TC-2.4-14) ──────────────────

    #[test]
    fn above_threshold_triggers() {
        // threshold = 200k - 20k - 13k = 167k
        let config = default_config();
        assert!(should_autocompact(170_000, &config));
    }

    #[test]
    fn below_threshold_does_not_trigger() {
        let config = default_config();
        assert!(!should_autocompact(160_000, &config));
    }

    #[test]
    fn at_exact_threshold_triggers() {
        let config = default_config();
        assert!(should_autocompact(167_000, &config));
    }

    #[test]
    fn disabled_config_never_triggers() {
        let config = CompactConfig {
            enabled: false,
            ..default_config()
        };
        assert!(!should_autocompact(999_999, &config));
    }

    #[test]
    fn custom_config_threshold() {
        let config = CompactConfig {
            context_window: 100_000,
            output_reserve: 10_000,
            autocompact_buffer: 5_000,
            ..default_config()
        };
        // threshold = 100k - 10k - 5k = 85k
        assert!(!should_autocompact(80_000, &config));
        assert!(should_autocompact(85_000, &config));
        assert!(should_autocompact(90_000, &config));
    }

    #[test]
    fn zero_tokens_does_not_trigger() {
        let config = default_config();
        assert!(!should_autocompact(0, &config));
    }

    #[test]
    fn threshold_pct_overrides_default_calculation() {
        let config = CompactConfig {
            context_window: 200_000,
            autocompact_threshold_pct: Some(50),
            ..default_config()
        };
        // threshold = 200k * 50 / 100 = 100k
        assert!(!should_autocompact(99_999, &config));
        assert!(should_autocompact(100_000, &config));
        assert!(should_autocompact(150_000, &config));
    }

    #[test]
    fn threshold_pct_zero_triggers_immediately() {
        let config = CompactConfig {
            autocompact_threshold_pct: Some(0),
            ..default_config()
        };
        // threshold = 0, any non-negative triggers
        assert!(should_autocompact(0, &config));
        assert!(should_autocompact(1, &config));
    }

    #[test]
    fn threshold_pct_100_never_triggers() {
        let config = CompactConfig {
            context_window: 200_000,
            autocompact_threshold_pct: Some(100),
            ..default_config()
        };
        // threshold = 200k, provider never reports 200k input_tokens
        assert!(!should_autocompact(199_999, &config));
        assert!(should_autocompact(200_000, &config));
    }

    #[test]
    fn threshold_pct_none_uses_default_logic() {
        let config = CompactConfig {
            autocompact_threshold_pct: None,
            ..default_config()
        };
        // Same as default: threshold = 200k - 20k - 13k = 167k
        assert!(!should_autocompact(166_999, &config));
        assert!(should_autocompact(167_000, &config));
    }

    // ── truncate_for_retry ──────────────────────────────────────────────

    #[test]
    fn truncate_drops_20_percent() {
        let msgs: Vec<Message> = (0..10)
            .map(|i| {
                let role = if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                Message::new(
                    role,
                    vec![ContentBlock::Text {
                        text: format!("msg-{i}"),
                    }],
                )
            })
            .collect();

        let result = truncate_for_retry(&msgs).unwrap();
        // Drop 20% of 10 = 2 messages, remaining 8
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn truncate_ensures_user_first() {
        let msgs: Vec<Message> = (0..5)
            .map(|i| {
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::Text {
                        text: format!("msg-{i}"),
                    }],
                )
            })
            .collect();

        let result = truncate_for_retry(&msgs).unwrap();
        assert_eq!(result[0].role, Role::User);
    }

    #[test]
    fn truncate_too_few_returns_none() {
        let msgs = vec![Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "only one".to_string(),
            }],
        )];
        assert!(truncate_for_retry(&msgs).is_none());
    }

    #[test]
    fn truncate_empty_returns_none() {
        assert!(truncate_for_retry(&[]).is_none());
    }

    #[test]
    fn truncate_preserves_user_first_without_placeholder() {
        // First remaining message is already User — no placeholder needed
        let msgs: Vec<Message> = (0..10)
            .map(|i| {
                let role = if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                Message::new(
                    role,
                    vec![ContentBlock::Text {
                        text: format!("msg-{i}"),
                    }],
                )
            })
            .collect();

        let result = truncate_for_retry(&msgs).unwrap();
        // msgs[2] (User) should be first; no placeholder prepended
        assert_eq!(result.len(), 8);
        match &result[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "msg-2"),
            _ => panic!("expected Text"),
        }
    }

    // ── boundary detection / extraction ─────────────────────────────────

    #[test]
    fn detect_boundary_message() {
        let metadata = CompactMetadata {
            trigger: CompactTrigger::Auto,
            pre_compact_tokens: 150_000,
            messages_summarized: 42,
        };
        let text = format!(
            "{BOUNDARY_PREFIX}\n{}",
            serde_json::to_string(&metadata).unwrap()
        );
        let msg = Message::new(Role::User, vec![ContentBlock::Text { text }]);
        assert!(is_compact_boundary(&msg));
    }

    #[test]
    fn non_boundary_message() {
        let msg = Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        );
        assert!(!is_compact_boundary(&msg));
    }

    #[test]
    fn extract_metadata_from_boundary() {
        let metadata = CompactMetadata {
            trigger: CompactTrigger::Auto,
            pre_compact_tokens: 150_000,
            messages_summarized: 42,
        };
        let text = format!(
            "{BOUNDARY_PREFIX}\n{}",
            serde_json::to_string(&metadata).unwrap()
        );
        let msg = Message::new(Role::User, vec![ContentBlock::Text { text }]);
        let extracted = extract_compact_metadata(&msg).unwrap();
        assert_eq!(extracted, metadata);
    }

    #[test]
    fn extract_metadata_from_non_boundary_returns_none() {
        let msg = Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "not a boundary".to_string(),
            }],
        );
        assert!(extract_compact_metadata(&msg).is_none());
    }
}
