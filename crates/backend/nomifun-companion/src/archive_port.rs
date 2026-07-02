//! Production [`ArchiveConversationPort`] over the conversation domain.
//!
//! Lives here (not in `archiver.rs`) so the archiver stays free of any
//! conversation-crate dependency and testable with a mock port. This adapter
//! reads a window's chat messages and durably resets the live engine context.

use std::sync::Arc;

use nomifun_ai_agent::IWorkerTaskManager;
use nomifun_api_types::ListMessagesQuery;
use nomifun_common::{AppError, MessagePosition, MessageType};
use nomifun_conversation::ConversationService;

use crate::archiver::{ArchiveConversationPort, WindowMessage};
use crate::companion::COMPANION_USER_ID;

/// Upper bound on messages pulled for one digest — a chatty window only needs
/// its most-recent turns summarized (the digest prompt caps again). Keeping this
/// bounded also bounds the initial (boundary=0) bootstrap over a legacy thread.
const FETCH_LIMIT: u32 = 400;

pub struct ConversationArchivePort {
    conversations: Arc<ConversationService>,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

impl ConversationArchivePort {
    pub fn new(conversations: Arc<ConversationService>, task_manager: Arc<dyn IWorkerTaskManager>) -> Self {
        Self { conversations, task_manager }
    }
}

/// Best-effort plain-text extraction from a message's `content` JSON. Companion
/// chat text lives either as a bare string or under `text`/`content`; anything
/// else (structured tool payloads) yields empty and is dropped by the caller.
fn extract_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

#[async_trait::async_trait]
impl ArchiveConversationPort for ConversationArchivePort {
    async fn window_messages(&self, conversation_id: &str, since_ts: i64) -> Result<Vec<WindowMessage>, AppError> {
        // Keyset "latest window" (oldest-first within the window) bounds memory
        // to the newest FETCH_LIMIT turns — exactly what the digest wants, and
        // it also surfaces the latest timestamp the archiver uses for idle.
        let query = ListMessagesQuery {
            cursor: Some(String::new()),
            page_size: Some(FETCH_LIMIT),
            ..Default::default()
        };
        let resp = self.conversations.list_messages(COMPANION_USER_ID, conversation_id, query).await?;
        let mut out = Vec::new();
        for m in resp.items {
            if m.hidden || m.created_at <= since_ts {
                continue;
            }
            // Only human/companion chat prose feeds the digest; tool calls,
            // thinking, status, plans etc. are engine noise, not conversation.
            if m.r#type != MessageType::Text {
                continue;
            }
            let is_user = match m.position {
                Some(MessagePosition::Right) => true,
                Some(MessagePosition::Left) => false,
                // Center/Pop = system/tips — not part of the human↔companion dialog.
                _ => continue,
            };
            let content = extract_text(&m.content);
            if content.trim().is_empty() {
                continue;
            }
            out.push(WindowMessage { is_user, content, created_at: m.created_at });
        }
        Ok(out)
    }

    async fn reset_context(&self, conversation_id: &str) -> Result<(), AppError> {
        // Warm the agent first so the reset is DURABLE: clear_context only clears
        // (and persists empty) a LIVE engine; an idle/unloaded Nomi agent would
        // otherwise resume its full on-disk session on the next build. warmup
        // builds the task without sending a message; clear_context then empties
        // and persists it.
        self.conversations
            .warmup(COMPANION_USER_ID, conversation_id, &self.task_manager)
            .await?;
        self.conversations
            .clear_context(COMPANION_USER_ID, conversation_id, &self.task_manager)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_handles_string_object_and_other() {
        assert_eq!(extract_text(&serde_json::json!("你好")), "你好");
        assert_eq!(extract_text(&serde_json::json!({"text": "帮我看看"})), "帮我看看");
        assert_eq!(extract_text(&serde_json::json!({"content": "备用字段"})), "备用字段");
        assert_eq!(extract_text(&serde_json::json!({"tool": "bash"})), "");
        assert_eq!(extract_text(&serde_json::json!(42)), "");
    }
}
