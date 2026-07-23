use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `channel_plugins` table.
///
/// One row per connected bot. The `config` column holds an encrypted JSON blob
/// containing credentials and options.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelPluginRow {
    pub channel_plugin_id: String,
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

/// Values accepted when inserting a `channel_plugins` row.
///
/// SQLite owns the technical `id`; callers address the row only through the
/// generated `channel_plugin_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewChannelPluginRow {
    pub r#type: String,
    pub name: String,
    pub enabled: bool,
    pub config: String,
    pub status: Option<String>,
    pub last_connected: Option<TimestampMs>,
    pub companion_id: Option<String>,
    pub public_agent_id: Option<String>,
    pub bot_key: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `channel_users` table.
///
/// Represents an IM user authorized to chat with the Agent.
/// UNIQUE constraint on (platform_user_id, platform_type, channel_plugin_id).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelUserRow {
    pub channel_user_id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    /// Optional logical reference to the `channel_plugins` business identity that owns
    /// this authorization. `None` means the authorization is not plugin-scoped.
    pub channel_plugin_id: Option<String>,
    pub display_name: Option<String>,
    pub authorized_at: TimestampMs,
    pub last_active: Option<TimestampMs>,
}

/// Values accepted when inserting a `channel_users` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewChannelUserRow {
    pub platform_user_id: String,
    pub platform_type: String,
    pub channel_plugin_id: Option<String>,
    pub display_name: Option<String>,
    pub authorized_at: TimestampMs,
    pub last_active: Option<TimestampMs>,
}

/// Row mapping for the `channel_sessions` table.
///
/// Per-chat session linking an authorized user to a conversation. Relations
/// are application-enforced logical references.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelSessionRow {
    pub channel_session_id: String,
    pub channel_user_id: String,
    pub agent_type: String,
    pub conversation_id: Option<String>,
    pub workspace: Option<String>,
    pub chat_id: Option<String>,
    /// The `channel_plugins` business identity this session arrived through. Two bots
    /// in the same chat get isolated sessions.
    pub channel_plugin_id: Option<String>,
    pub created_at: TimestampMs,
    pub last_activity: TimestampMs,
}

/// Values accepted when inserting a `channel_sessions` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewChannelSessionRow {
    pub channel_session_id: String,
    pub channel_user_id: String,
    pub agent_type: String,
    pub conversation_id: Option<String>,
    pub workspace: Option<String>,
    pub chat_id: Option<String>,
    pub channel_plugin_id: Option<String>,
    pub created_at: TimestampMs,
    pub last_activity: TimestampMs,
}

/// Row mapping for the `channel_pairing_codes` table.
///
/// 6-digit pairing code with 10-minute expiry. Status transitions:
/// pending → approved | rejected | expired.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelPairingCodeRow {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    /// The bot channel this pairing was initiated through.
    pub channel_plugin_id: Option<String>,
    pub display_name: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub status: String,
}

/// Values accepted when inserting a `channel_pairing_codes` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewChannelPairingCodeRow {
    pub code: String,
    pub platform_user_id: String,
    pub platform_type: String,
    pub channel_plugin_id: Option<String>,
    pub display_name: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub status: String,
}
