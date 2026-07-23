use nomifun_common::TimestampMs;

use crate::error::DbError;
use crate::models::{
    ChannelPairingCodeRow, ChannelPluginRow, ChannelSessionRow, ChannelUserRow,
    NewChannelPairingCodeRow, NewChannelPluginRow, NewChannelSessionRow, NewChannelUserRow,
};

/// Data access abstraction for channel integration tables.
///
/// Covers four tables: `channel_plugins`, `channel_users`,
/// `channel_sessions`, and `channel_pairing_codes`.
///
/// Object-safe via `async_trait` to support `Arc<dyn IChannelRepository>`.
#[async_trait::async_trait]
pub trait IChannelRepository: Send + Sync {
    // ── Plugin CRUD ──────────────────────────────────────────────────

    /// Returns all registered plugins.
    async fn get_all_plugins(&self) -> Result<Vec<ChannelPluginRow>, DbError>;

    /// Returns a single plugin by business id, or `None` if not found.
    async fn get_plugin(&self, channel_plugin_id: &str) -> Result<Option<ChannelPluginRow>, DbError>;

    /// Inserts a plugin and returns the persisted row with its generated UUIDv7.
    async fn create_plugin(&self, row: &NewChannelPluginRow) -> Result<ChannelPluginRow, DbError>;

    /// Updates an existing plugin by its business id.
    async fn update_plugin(&self, row: &ChannelPluginRow) -> Result<ChannelPluginRow, DbError>;

    /// Updates only the `status` and `last_connected` of a plugin.
    async fn update_plugin_status(
        &self,
        channel_plugin_id: &str,
        params: &UpdatePluginStatusParams,
    ) -> Result<(), DbError>;

    /// Updates the companion binding of a plugin row (`None` clears it).
    ///
    /// Row-level mutual exclusivity: setting a non-null `companion_id` also
    /// clears any `public_agent_id` on the same row (a bot serves EITHER a
    /// companion OR a public agent, never both).
    async fn update_plugin_companion(
        &self,
        channel_plugin_id: &str,
        companion_id: Option<&str>,
    ) -> Result<(), DbError>;

    /// Updates the 对外伙伴 (public agent) binding of a plugin row (`None`
    /// clears it). Row-level mutual exclusivity: setting a non-null
    /// `public_agent_id` also clears any `companion_id` on the same row.
    async fn update_plugin_public_agent(
        &self,
        channel_plugin_id: &str,
        public_agent_id: Option<&str>,
    ) -> Result<(), DbError>;

    /// Backfills or rotates the stable platform bot identity for a plugin.
    async fn update_plugin_bot_key(
        &self,
        channel_plugin_id: &str,
        bot_key: &str,
    ) -> Result<(), DbError>;

    /// Deletes a plugin by business id. Returns `DbError::NotFound` if absent.
    async fn delete_plugin(&self, channel_plugin_id: &str) -> Result<(), DbError>;

    // ── User CRUD ────────────────────────────────────────────────────

    /// Returns all authorized users.
    async fn get_all_users(&self) -> Result<Vec<ChannelUserRow>, DbError>;

    /// Finds a user by platform identity scoped to one bot channel.
    async fn get_user_by_platform(
        &self,
        platform_user_id: &str,
        platform_type: &str,
        channel_plugin_id: &str,
    ) -> Result<Option<ChannelUserRow>, DbError>;

    /// Creates an authorized user and returns its generated UUIDv7.
    async fn create_user(&self, row: &NewChannelUserRow) -> Result<ChannelUserRow, DbError>;

    /// Updates `last_active` timestamp for a user.
    async fn update_user_last_active(
        &self,
        channel_user_id: &str,
        last_active: TimestampMs,
    ) -> Result<(), DbError>;

    /// Deletes a user and its associated sessions transactionally by business
    /// id. Returns `DbError::NotFound` if absent.
    async fn delete_user(&self, channel_user_id: &str) -> Result<(), DbError>;

    // ── Session CRUD ─────────────────────────────────────────────────

    /// Returns all sessions.
    async fn get_all_sessions(&self) -> Result<Vec<ChannelSessionRow>, DbError>;

    /// Returns a single session by id.
    async fn get_session(&self, channel_session_id: &str) -> Result<Option<ChannelSessionRow>, DbError>;

    /// Finds an existing session by channel + user + chat, or creates a new
    /// one. If found, updates `last_activity` and returns the existing row.
    /// If not found, inserts `new_row` and returns it.
    async fn get_or_create_session(
        &self,
        channel_user_id: &str,
        chat_id: &str,
        channel_plugin_id: &str,
        new_row: &NewChannelSessionRow,
    ) -> Result<ChannelSessionRow, DbError>;

    /// Updates `last_activity` timestamp for a session.
    async fn update_session_activity(&self, channel_session_id: &str, last_activity: TimestampMs) -> Result<(), DbError>;

    /// Updates the `conversation_id` of a session.
    async fn update_session_conversation(&self, channel_session_id: &str, conversation_id: &str) -> Result<(), DbError>;

    /// Updates the `agent_type` of a session.
    async fn update_session_agent_type(&self, channel_session_id: &str, agent_type: &str) -> Result<(), DbError>;

    /// Deletes all sessions belonging to a user.
    async fn delete_sessions_by_user(&self, channel_user_id: &str) -> Result<(), DbError>;

    /// Deletes all sessions that arrived through a channel row.
    async fn delete_sessions_by_channel(&self, channel_plugin_id: &str) -> Result<(), DbError>;

    /// Deletes the session for a specific channel + user + chat triple.
    async fn delete_session_by_user_chat(
        &self,
        channel_user_id: &str,
        chat_id: &str,
        channel_plugin_id: &str,
    ) -> Result<(), DbError>;

    // ── Pairing Codes ────────────────────────────────────────────────

    /// Creates a pairing code and returns its SQLite-assigned id.
    async fn create_pairing(&self, row: &NewChannelPairingCodeRow) -> Result<ChannelPairingCodeRow, DbError>;

    /// Returns all pairing codes with status = 'pending'.
    async fn get_pending_pairings(&self) -> Result<Vec<ChannelPairingCodeRow>, DbError>;

    /// Retrieves a single pairing code, or `None` if not found.
    async fn get_pairing_by_code(&self, code: &str) -> Result<Option<ChannelPairingCodeRow>, DbError>;

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
