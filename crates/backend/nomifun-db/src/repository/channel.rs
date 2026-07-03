use nomifun_common::TimestampMs;

use crate::error::DbError;
use crate::models::{AssistantSessionRow, AssistantUserRow, ChannelPluginRow, PairingCodeRow};

/// Data access abstraction for channel integration tables.
///
/// Covers four tables: `assistant_plugins`, `assistant_users`,
/// `assistant_sessions`, and `assistant_pairing_codes`.
///
/// Object-safe via `async_trait` to support `Arc<dyn IChannelRepository>`.
#[async_trait::async_trait]
pub trait IChannelRepository: Send + Sync {
    // ── Plugin CRUD ──────────────────────────────────────────────────

    /// Returns all registered plugins.
    async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError>;

    /// Returns a single plugin by id, or `None` if not found.
    async fn get_plugin(&self, id: &str) -> Result<Option<ChannelPluginRow>, DbError>;

    /// Inserts a new plugin or updates an existing one (by id).
    async fn upsert_plugin(&self, row: &ChannelPluginRow) -> Result<(), DbError>;

    /// Updates only the `status` and `last_connected` of a plugin.
    async fn update_plugin_status(&self, id: &str, params: &UpdatePluginStatusParams) -> Result<(), DbError>;

    /// Updates the companion binding of a plugin row (`None` clears it).
    ///
    /// Row-level mutual exclusivity: setting a non-null `companion_id` also
    /// clears any `public_agent_id` on the same row (a bot serves EITHER a
    /// companion OR a public agent, never both).
    async fn update_plugin_companion(&self, id: &str, companion_id: Option<&str>) -> Result<(), DbError>;

    /// Updates the 对外伙伴 (public agent) binding of a plugin row (`None`
    /// clears it). Row-level mutual exclusivity: setting a non-null
    /// `public_agent_id` also clears any `companion_id` on the same row.
    async fn update_plugin_public_agent(&self, id: &str, public_agent_id: Option<&str>) -> Result<(), DbError>;

    /// Updates the bot identity key of a plugin row (backfill on restore).
    async fn update_plugin_bot_key(&self, id: &str, bot_key: &str) -> Result<(), DbError>;

    /// Deletes a plugin by id. Returns `DbError::NotFound` if absent.
    async fn delete_plugin(&self, id: &str) -> Result<(), DbError>;

    // ── User CRUD ────────────────────────────────────────────────────

    /// Returns all authorized users.
    async fn get_all_users(&self) -> Result<Vec<AssistantUserRow>, DbError>;

    /// Finds a user by platform identity scoped to one bot channel.
    async fn get_user_by_platform(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_id: &str,
    ) -> Result<Option<AssistantUserRow>, DbError>;

    /// Creates a new authorized user record.
    async fn create_user(&self, row: &AssistantUserRow) -> Result<(), DbError>;

    /// Updates `last_active` timestamp for a user.
    async fn update_user_last_active(&self, id: &str, last_active: TimestampMs) -> Result<(), DbError>;

    /// Deletes a user by id. Returns `DbError::NotFound` if absent.
    /// Associated sessions are cascade-deleted by the database.
    async fn delete_user(&self, id: &str) -> Result<(), DbError>;

    // ── Session CRUD ─────────────────────────────────────────────────

    /// Returns all sessions.
    async fn get_all_sessions(&self) -> Result<Vec<AssistantSessionRow>, DbError>;

    /// Returns a single session by id.
    async fn get_session(&self, id: &str) -> Result<Option<AssistantSessionRow>, DbError>;

    /// Finds an existing session by channel + user + chat, or creates a new
    /// one. If found, updates `last_activity` and returns the existing row.
    /// If not found, inserts `new_row` and returns it.
    async fn get_or_create_session(
        &self,
        user_id: &str,
        chat_id: &str,
        channel_id: &str,
        new_row: &AssistantSessionRow,
    ) -> Result<AssistantSessionRow, DbError>;

    /// Updates `last_activity` timestamp for a session.
    async fn update_session_activity(&self, id: &str, last_activity: TimestampMs) -> Result<(), DbError>;

    /// Updates the `conversation_id` of a session.
    async fn update_session_conversation(&self, id: &str, conversation_id: i64) -> Result<(), DbError>;

    /// Updates the `agent_type` of a session.
    async fn update_session_agent_type(&self, id: &str, agent_type: &str) -> Result<(), DbError>;

    /// Deletes all sessions belonging to a user.
    async fn delete_sessions_by_user(&self, user_id: &str) -> Result<(), DbError>;

    /// Deletes all sessions that arrived through a channel row.
    async fn delete_sessions_by_channel(&self, channel_id: &str) -> Result<(), DbError>;

    /// Deletes the session for a specific channel + user + chat triple.
    async fn delete_session_by_user_chat(&self, user_id: &str, chat_id: &str, channel_id: &str)
    -> Result<(), DbError>;

    // ── Pairing Codes ────────────────────────────────────────────────

    /// Creates a new pairing code record.
    async fn create_pairing(&self, row: &PairingCodeRow) -> Result<(), DbError>;

    /// Returns all pairing codes with status = 'pending'.
    async fn get_pending_pairings(&self) -> Result<Vec<PairingCodeRow>, DbError>;

    /// Retrieves a single pairing code, or `None` if not found.
    async fn get_pairing_by_code(&self, code: &str) -> Result<Option<PairingCodeRow>, DbError>;

    /// Updates the status of a pairing code.
    /// Returns `DbError::NotFound` if the code doesn't exist.
    async fn update_pairing_status(&self, code: &str, status: &str) -> Result<(), DbError>;

    /// Marks all expired-but-still-pending pairing codes as 'expired'.
    /// `now` is the current timestamp in milliseconds.
    async fn cleanup_expired_pairings(&self, now: TimestampMs) -> Result<u64, DbError>;
}

/// Parameters for updating plugin runtime status.
#[derive(Debug, Clone, Default)]
pub struct UpdatePluginStatusParams {
    pub status: Option<String>,
    pub last_connected: Option<TimestampMs>,
    pub enabled: Option<bool>,
}
