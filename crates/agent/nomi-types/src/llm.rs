use serde_json::Value;

use crate::message::{StopReason, TokenUsage, ToolUseId};
use crate::tool::ToolDef;

/// A request to the LLM provider
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<crate::message::Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Optional: thinking config (Anthropic extended thinking)
    pub thinking: Option<ThinkingConfig>,
    /// Optional: reasoning effort for OpenAI reasoning models (low/medium/high)
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ThinkingConfig {
    Enabled { budget_tokens: u32 },
    Disabled,
}

/// Streaming events from the LLM
#[derive(Debug, Clone)]
pub enum LlmEvent {
    /// Incremental text output
    TextDelta(String),
    /// Complete tool call (after accumulating streaming deltas)
    ToolUse {
        id: ToolUseId,
        name: String,
        input: Value,
        /// Opaque provider metadata (e.g. Gemini thought_signature) to round-trip.
        extra: Option<Value>,
    },
    /// Partial tool call progress while the provider is still streaming tool arguments.
    ToolUseDelta {
        id: ToolUseId,
        name: String,
        /// Small structured preview of arguments that are already known.
        input: Option<Value>,
    },
    /// Thinking content (Anthropic only)
    ThinkingDelta(String),
    /// Opaque provider signature for the current thinking block.
    ThinkingSignature(String),
    /// Response complete
    Done {
        stop_reason: StopReason,
        usage: TokenUsage,
    },
    /// Error from the API
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{StopReason, TokenUsage};
    use serde_json::json;

    #[test]
    fn test_thinking_config_enabled_stores_budget() {
        let config = ThinkingConfig::Enabled {
            budget_tokens: 4096,
        };
        match config {
            ThinkingConfig::Enabled { budget_tokens } => assert_eq!(budget_tokens, 4096),
            ThinkingConfig::Disabled => panic!("expected Enabled"),
        }
    }

    #[test]
    fn test_llm_event_text_delta_carries_content() {
        let event = LlmEvent::TextDelta("hello".to_string());
        match event {
            LlmEvent::TextDelta(text) => assert_eq!(text, "hello"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn test_llm_event_done_carries_stop_reason_and_usage() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_tokens: 0,
            cache_read_tokens: 5,
        };
        let event = LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage,
        };
        match event {
            LlmEvent::Done { stop_reason, usage } => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 10);
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn test_llm_event_tool_use_fields() {
        let event = LlmEvent::ToolUse {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            input: json!({"cmd": "ls"}),
            extra: None,
        };
        match &event {
            LlmEvent::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(input["cmd"], "ls");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_llm_event_tool_use_delta_fields() {
        let event = LlmEvent::ToolUseDelta {
            id: "call_1".to_string(),
            name: "Write".to_string(),
            input: Some(json!({"file_path": "snake.html"})),
        };

        match &event {
            LlmEvent::ToolUseDelta { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "Write");
                assert_eq!(input.as_ref().unwrap()["file_path"], "snake.html");
            }
            _ => panic!("expected ToolUseDelta"),
        }
    }

    #[test]
    fn test_llm_event_thinking_signature_carries_content() {
        let event = LlmEvent::ThinkingSignature("sig-123".to_string());

        match event {
            LlmEvent::ThinkingSignature(signature) => assert_eq!(signature, "sig-123"),
            _ => panic!("expected ThinkingSignature"),
        }
    }
}
