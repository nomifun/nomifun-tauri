use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `conversations` table.
///
/// Enum-like fields (`type`, `status`, `source`) are stored as TEXT strings.
/// The service layer converts them to/from `nomifun_common` enums
/// (`AgentType`, `ConversationStatus`, `ConversationSource`).
///
/// JSON fields (`extra`, `model`) are stored as TEXT in SQLite and
/// deserialized by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConversationRow {
    pub id: i64,
    pub conversation_id: String,
    pub user_id: String,
    pub name: String,
    /// Agent type string (e.g. "gemini", "acp", "remote").
    #[sqlx(rename = "type")]
    pub r#type: String,
    /// JSON object: type-specific extra data.
    pub extra: String,
    pub delegation_policy: String,
    /// Tagged `ExecutionModelPool` JSON. NULL means no collaboration override
    /// (the Gateway inherits the Conversation lead model); explicit
    /// `{ "mode": "automatic" }` retains automatic catalog selection.
    pub execution_model_pool: Option<String>,
    pub decision_policy: String,
    /// Reusable authoring configuration selected for the next top-level
    /// collaboration launch. Existing Executions contain frozen snapshots.
    pub execution_template_id: Option<String>,
    /// JSON object: `ProviderWithModel` serialized.
    pub model: Option<String>,
    /// One of: "pending", "running", "finished". NULL means unspecified.
    pub status: Option<String>,
    /// One of: "nomifun", "telegram", "lark", "dingtalk", "weixin".
    pub source: Option<String>,
    /// Channel isolation ID (e.g. "user:xxx", "group:xxx").
    pub channel_chat_id: Option<String>,
    /// Whether this conversation is pinned (SQLite INTEGER 0/1).
    pub pinned: bool,
    pub pinned_at: Option<TimestampMs>,
    /// UUIDv7 logical reference to the cron job that created this conversation.
    pub cron_job_id: Option<String>,
    /// Preset lineage and immutable resolved launch configuration.
    pub preset_id: Option<String>,
    pub preset_revision: Option<i64>,
    pub preset_snapshot: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConversationDeliveryReceiptRow {
    pub id: i64,
    pub operation_id: String,
    /// Immutable message identity in the idempotency scope. This survives
    /// transcript reset and is returned to all replays.
    pub message_id: String,
    /// Immutable Conversation identity in the idempotency scope.
    pub conversation_id: String,
    pub user_id: String,
    pub kind: String,
    pub request_payload: String,
    pub status: String,
    pub result_ok: Option<bool>,
    pub result_text: Option<String>,
    pub result_error: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub completed_at: Option<TimestampMs>,
    /// Nullable links to the currently materialized aggregate. Reset/clear
    /// detaches these without deleting the replay receipt.
    pub projected_conversation_id: Option<String>,
    pub projected_message_id: Option<String>,
}
