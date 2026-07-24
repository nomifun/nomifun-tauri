use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `messages` table.
///
/// Enum-like fields (`type`, `position`, `status`) are stored as TEXT strings.
/// The service layer converts them to/from `nomifun_common` enums
/// (`MessageType`, `MessagePosition`, `MessageStatus`).
///
/// The `content` field is a JSON TEXT column deserialized by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MessageRow {
    pub id: i64,
    pub message_id: String,
    pub conversation_id: String,
    /// Source message ID for streaming message merge identification.
    pub msg_id: Option<String>,
    /// Message type string (e.g. "text", "tips", "tool_call").
    #[sqlx(rename = "type")]
    pub r#type: String,
    /// JSON object: type-specific message content.
    pub content: String,
    /// One of: "left", "right", "center", "pop".
    pub position: Option<String>,
    /// One of: "finish", "pending", "error", "work".
    pub status: Option<String>,
    /// Whether this message is hidden (SQLite INTEGER 0/1).
    pub hidden: bool,
    pub created_at: TimestampMs,
}
