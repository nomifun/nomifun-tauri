use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `assistant_plugins` table.
///
/// One row per connected bot — multiple rows may share the same platform
/// `type` (legacy rows keep `id == type`). The `config` column holds an
/// encrypted JSON blob containing credentials and options.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelPluginRow {
    pub id: String,
    /// Platform type (telegram, lark, dingtalk, weixin, slack, discord).
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub name: String,
    pub enabled: bool,
    /// JSON blob: `{ credentials, config }`. Stored encrypted at rest.
    pub config: String,
    pub status: Option<String>,
    pub last_connected: Option<TimestampMs>,
    /// Companion bound to this bot. UNIQUE(type, bot_key) guarantees a bot is
    /// never bound to more than one companion.
    pub companion_id: Option<String>,
    /// 对外伙伴 (public agent) bound to this bot. Row-level mutually exclusive
    /// with `companion_id`: a bot serves EITHER a companion OR a public agent OR
    /// nothing, never both (enforced in the repository/manager layer).
    pub public_agent_id: Option<String>,
    /// Platform-level bot identity (lark app_id, telegram bot id, ...),
    /// extracted from credentials on enable/restore.
    pub bot_key: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `assistant_users` table.
///
/// Represents an IM user authorized to chat with the assistant.
/// UNIQUE constraint on (platform_user_id, platform_type).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantUserRow {
    pub id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    /// The `assistant_plugins` row (bot) this authorization belongs to.
    /// `None` only for legacy rows the 004 migration could not backfill.
    pub channel_id: Option<String>,
    pub display_name: Option<String>,
    pub authorized_at: TimestampMs,
    pub last_active: Option<TimestampMs>,
    pub session_id: Option<String>,
}

/// Row mapping for the `assistant_sessions` table.
///
/// Per-chat session linking an authorized user to a conversation.
/// FK: user_id → assistant_users(id) ON DELETE CASCADE.
/// FK: conversation_id → conversations(id) ON DELETE SET NULL.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantSessionRow {
    pub id: String,
    pub user_id: String,
    pub agent_type: String,
    pub conversation_id: Option<i64>,
    pub workspace: Option<String>,
    pub chat_id: Option<String>,
    /// The `assistant_plugins` row this session arrived through. Two bots
    /// in the same chat get isolated sessions.
    pub channel_id: Option<String>,
    pub created_at: TimestampMs,
    pub last_activity: TimestampMs,
}

/// Row mapping for the `assistant_pairing_codes` table.
///
/// 6-digit pairing code with 10-minute expiry. Status transitions:
/// pending → approved | rejected | expired.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PairingCodeRow {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    /// The bot channel this pairing was initiated through.
    pub channel_id: Option<String>,
    pub display_name: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub status: String,
}
