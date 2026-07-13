//! Sanitize a resumed nomi session's message history before it is replayed
//! to a provider.
//!
//! Background: when the user clicks "Stop" on a tool-call mid-stream, nomi
//! may persist an assistant message that contains `ToolUse` content blocks
//! but whose tool calls were never followed up by the matching `ToolResult`
//! blocks. On the next turn, the engine replays history verbatim and strict
//! providers reject the request:
//!   - Ollama-compatible providers (e.g. `qwen3:8b`) return
//!     `400 invalid message content type: <nil>` because the assistant
//!     message has `tool_calls != null` but `content == null`.
//!   - Some OpenAI-compatible proxies (e.g. DeepSeek behind a strict gateway)
//!     return `400 invalid_request_error` for the same reason.
//!
//! Fix: repair tool calls as ordered adjacent pairs. A result may answer only
//! the assistant tool-call group immediately before it; a matching id later in
//! history must not make a broken transcript appear valid. Historical tool
//! screenshots are also removed before replay, and provider-specific thinking
//! blocks are removed when switching providers.
//!
//! This logic is intentionally a free function (not a method on
//! `NomiAgentManager`) so it can be unit-tested in isolation and so we do
//! not add yet another field to a manager (per `AGENTS.md`).

use std::collections::{HashMap, HashSet};

use nomi_types::message::{ContentBlock, Message, Role};

const HISTORICAL_IMAGE_NOTE: &str =
    "(Image attachment omitted during historical session recovery; capture a fresh observation if needed.)";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionRepairStats {
    pub removed_messages: usize,
    pub removed_tool_calls: usize,
    pub removed_tool_results: usize,
    pub removed_images: usize,
    pub removed_thinking: usize,
}

/// Repair a resumed transcript to the provider-safe subset of its history.
///
/// A tool result is valid only when it belongs to the immediately preceding
/// assistant tool-call group. Operates in-place and is safe on empty input.
pub fn sanitize_session_messages(
    messages: &mut Vec<Message>,
    provider_changed: bool,
) -> SessionRepairStats {
    if messages.is_empty() {
        return SessionRepairStats::default();
    }

    // Track validity by message position, not only by id. Provider-generated
    // ids are normally unique, but legacy/corrupt histories can reuse one; a
    // later valid pair must never rescue an earlier orphan with the same id.
    let mut keep_tool_uses = vec![HashSet::new(); messages.len()];
    let mut keep_tool_results = vec![HashSet::new(); messages.len()];
    for index in 0..messages.len().saturating_sub(1) {
        let assistant = &messages[index];
        let result_group = &messages[index + 1];
        if assistant.role != Role::Assistant
            || !matches!(result_group.role, Role::User | Role::Tool)
        {
            continue;
        }
        let mut calls: HashMap<&str, Vec<usize>> = HashMap::new();
        for (block_index, block) in assistant.content.iter().enumerate() {
            if let ContentBlock::ToolUse { id, .. } = block {
                calls.entry(id.as_str()).or_default().push(block_index);
            }
        }
        let mut results: HashMap<&str, Vec<usize>> = HashMap::new();
        for (block_index, block) in result_group.content.iter().enumerate() {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                results.entry(tool_use_id.as_str()).or_default().push(block_index);
            }
        }
        for (id, call_positions) in calls {
            let Some(result_positions) = results.get(id) else {
                continue;
            };
            if !id.trim().is_empty() && call_positions.len() == 1 && result_positions.len() == 1 {
                keep_tool_uses[index].insert(call_positions[0]);
                keep_tool_results[index + 1].insert(result_positions[0]);
            }
        }
    }

    let mut stats = SessionRepairStats::default();
    let original_len = messages.len();
    for (index, message) in messages.iter_mut().enumerate() {
        let role = message.role;
        let old_content = std::mem::take(&mut message.content);
        message.content = old_content
            .into_iter()
            .enumerate()
            .filter_map(|(block_index, mut block)| {
                let keep = match &mut block {
            ContentBlock::ToolUse { .. } => {
                let keep = role == Role::Assistant && keep_tool_uses[index].contains(&block_index);
                stats.removed_tool_calls += usize::from(!keep);
                keep
            }
            ContentBlock::ToolResult {
                content,
                images,
                ..
            } => {
                let keep = matches!(role, Role::User | Role::Tool)
                    && keep_tool_results[index].contains(&block_index);
                if !keep {
                    stats.removed_tool_results += 1;
                    stats.removed_images += images.len();
                    false
                } else {
                    if !images.is_empty() {
                        stats.removed_images += images.len();
                        images.clear();
                        if !content.contains(HISTORICAL_IMAGE_NOTE) {
                            if !content.is_empty() {
                                content.push_str("\n\n");
                            }
                            content.push_str(HISTORICAL_IMAGE_NOTE);
                        }
                    }
                    true
                }
            }
            ContentBlock::Thinking { .. } if provider_changed => {
                stats.removed_thinking += 1;
                false
            }
            ContentBlock::Text { text } => {
                // Empty streamed deltas have no semantic value and otherwise
                // keep a fully-repaired message artificially alive.
                !text.trim().is_empty()
            }
                    ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => true,
                };
                keep.then_some(block)
            })
            .collect();
    }
    messages.retain(|message| !message.content.is_empty());
    stats.removed_messages = original_len - messages.len();
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_types::message::{Message, Role};
    use serde_json::json;

    fn assistant_tool_call(ids: &[&str]) -> Message {
        let blocks = ids
            .iter()
            .map(|id| ContentBlock::ToolUse {
                id: (*id).to_owned(),
                name: "search".to_owned(),
                input: json!({}),
                extra: None,
            })
            .collect();
        Message::new(Role::Assistant, blocks)
    }

    fn assistant_text_plus_tool_call(text: &str, id: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Text { text: text.to_owned() },
                ContentBlock::ToolUse {
                    id: id.to_owned(),
                    name: "search".to_owned(),
                    input: json!({}),
                    extra: None,
                },
            ],
        )
    }

    fn user_tool_result(tool_use_id: &str) -> Message {
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_owned(),
                content: "ok".to_owned(),
                is_error: false,
                images: Vec::new(),
            }],
        )
    }

    fn user_tool_results(tool_use_ids: &[&str]) -> Message {
        Message::new(
            Role::User,
            tool_use_ids
                .iter()
                .map(|tool_use_id| ContentBlock::ToolResult {
                    tool_use_id: (*tool_use_id).to_owned(),
                    content: "ok".to_owned(),
                    is_error: false,
                    images: Vec::new(),
                })
                .collect(),
        )
    }

    fn user_text(text: &str) -> Message {
        Message::new(Role::User, vec![ContentBlock::Text { text: text.to_owned() }])
    }

    fn assistant_text(text: &str) -> Message {
        Message::new(Role::Assistant, vec![ContentBlock::Text { text: text.to_owned() }])
    }

    #[test]
    fn drops_orphaned_assistant_tool_call_with_no_matching_result() {
        // user → assistant(tool_use, no text) — Stop pressed before tool_result
        let mut messages = vec![user_text("do thing"), assistant_tool_call(&["call_orphan"])];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
    }

    #[test]
    fn keeps_assistant_tool_call_with_matching_tool_result() {
        // user → assistant(tool_use) → user(tool_result) — valid pair
        let mut messages = vec![
            user_text("do thing"),
            assistant_tool_call(&["call_ok"]),
            user_tool_result("call_ok"),
        ];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 0);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn keeps_regular_assistant_text_message() {
        let mut messages = vec![user_text("hi"), assistant_text("hello there")];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 0);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn strips_orphan_tool_call_but_keeps_assistant_text() {
        let mut messages = vec![
            user_text("hi"),
            assistant_text_plus_tool_call("partial reply", "call_orphan"),
        ];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content.len(), 1);
        assert!(matches!(messages[1].content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn keeps_matched_call_and_result_when_sibling_call_is_orphaned() {
        let mut messages = vec![
            user_text("do two things"),
            assistant_tool_call(&["call_a", "call_b"]),
            user_tool_result("call_a"),
        ];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 0);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content.len(), 1);
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::ToolUse { id, .. } if id == "call_a"
        ));
    }

    #[test]
    fn keeps_full_history_when_all_pairs_match() {
        let mut messages = vec![
            user_text("first"),
            assistant_tool_call(&["c1"]),
            user_tool_result("c1"),
            assistant_text("done"),
            user_text("again"),
            assistant_tool_call(&["c2", "c3"]),
            user_tool_results(&["c2", "c3"]),
            assistant_text("all done"),
        ];
        let original_len = messages.len();
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 0);
        assert_eq!(messages.len(), original_len);
    }

    #[test]
    fn no_op_on_empty_history() {
        let mut messages: Vec<Message> = Vec::new();
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 0);
        assert!(messages.is_empty());
    }

    #[test]
    fn drops_orphan_assistant_with_only_empty_text_and_tool_call() {
        // Some providers stream an empty text delta before the tool call.
        // Empty/whitespace text should NOT save the assistant message.
        let msg = Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Text { text: "   ".to_owned() },
                ContentBlock::ToolUse {
                    id: "call_empty".to_owned(),
                    name: "search".to_owned(),
                    input: json!({}),
                    extra: None,
                },
            ],
        );
        let mut messages = vec![user_text("hi"), msg];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 1);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn drops_stray_tool_result_without_a_preceding_assistant_call() {
        let mut messages = vec![user_text("hi"), user_tool_result("stray")];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 1);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn late_tool_result_does_not_rescue_an_orphaned_call() {
        let mut messages = vec![
            user_text("do thing"),
            assistant_tool_call(&["late"]),
            assistant_text("intervening reply"),
            user_tool_result("late"),
        ];
        let removed = sanitize_session_messages(&mut messages, false).removed_messages;
        assert_eq!(removed, 2);
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|message| {
            message.content.iter().all(|block| {
                !matches!(block, ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. })
            })
        }));
    }

    #[test]
    fn later_pair_with_reused_id_does_not_rescue_earlier_orphans() {
        let mut messages = vec![
            assistant_tool_call(&["duplicate"]),
            assistant_text("break adjacency"),
            user_tool_result("duplicate"),
            assistant_tool_call(&["duplicate"]),
            user_tool_result("duplicate"),
        ];

        let stats = sanitize_session_messages(&mut messages, false);

        assert_eq!(stats.removed_tool_calls, 1);
        assert_eq!(stats.removed_tool_results, 1);
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[1].content[0], ContentBlock::ToolUse { .. }));
        assert!(matches!(messages[2].content[0], ContentBlock::ToolResult { .. }));
    }

    #[test]
    fn duplicate_ids_inside_one_group_are_removed_as_ambiguous() {
        let mut messages = vec![
            assistant_tool_call(&["duplicate", "duplicate"]),
            user_tool_result("duplicate"),
        ];

        let stats = sanitize_session_messages(&mut messages, false);

        assert_eq!(stats.removed_tool_calls, 2);
        assert_eq!(stats.removed_tool_results, 1);
        assert!(messages.is_empty());
    }

    #[test]
    fn empty_tool_ids_are_never_replayed() {
        for id in ["", "   ", "\t"] {
            let mut messages = vec![assistant_tool_call(&[id]), user_tool_result(id)];

            let stats = sanitize_session_messages(&mut messages, false);

            assert_eq!(stats.removed_tool_calls, 1, "id={id:?}");
            assert_eq!(stats.removed_tool_results, 1, "id={id:?}");
            assert!(messages.is_empty(), "id={id:?}");
        }
    }

    #[test]
    fn strips_images_from_completed_historical_tool_results() {
        let mut result = user_tool_result("shot");
        let ContentBlock::ToolResult { images, .. } = &mut result.content[0] else {
            unreachable!()
        };
        images.push(nomi_types::tool::ToolImage {
            media_type: "image/png".to_owned(),
            data: "large-base64".to_owned(),
        });
        let mut messages = vec![user_text("look"), assistant_tool_call(&["shot"]), result];
        sanitize_session_messages(&mut messages, false);
        let ContentBlock::ToolResult { content, images, .. } = &messages[2].content[0] else {
            panic!("expected tool result")
        };
        assert!(images.is_empty());
        assert!(content.contains("historical session recovery"));
    }

    #[test]
    fn provider_switch_strips_thinking_but_keeps_visible_text() {
        let mut messages = vec![Message::new(
            Role::Assistant,
            vec![
                ContentBlock::Thinking {
                    thinking: "provider scratchpad".to_owned(),
                    signature: Some("opaque-signature".to_owned()),
                },
                ContentBlock::Text {
                    text: "visible answer".to_owned(),
                },
            ],
        )];

        let stats = sanitize_session_messages(&mut messages, true);

        assert_eq!(stats.removed_thinking, 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(messages[0].content[0], ContentBlock::Text { .. }));
    }
}
