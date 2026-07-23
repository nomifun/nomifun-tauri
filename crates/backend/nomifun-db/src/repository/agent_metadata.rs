//! Repository trait for the `agent_metadata` table.

use crate::error::DbError;
use crate::models::{AgentMetadataRow, UpdateAgentHandshakeParams, UpsertAgentMetadataParams};

/// CRUD access for agent metadata rows.
///
/// The table is the single source of truth for how each agent is spawned
/// and what static capabilities it exposes. Handshake-derived fields
/// (`agent_capabilities`, `auth_methods`, `config_options`,
/// `available_modes`, `available_models`, `available_commands`) are
/// refreshed separately via [`IAgentMetadataRepository::apply_handshake`].
#[async_trait::async_trait]
pub trait IAgentMetadataRepository: Send + Sync {
    /// Return every row, in insertion order.
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError>;

    /// Look up by the durable business `agent_id`.
    async fn get(&self, agent_id: &str) -> Result<Option<AgentMetadataRow>, DbError>;

    /// Look up by the unique `(agent_source, name)` pair.
    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError>;

    /// Look up the first `builtin` row whose vendor label matches.
    /// Useful when the caller only has the legacy `backend` string and
    /// not a full agent id.
    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError>;

    /// Insert or replace a row. Returns the row as stored.
    async fn upsert(&self, params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError>;

    /// Apply handshake-derived fields on top of an existing row.
    /// Returns `Ok(None)` if no row matches `agent_id`.
    async fn apply_handshake(
        &self,
        agent_id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError>;

    /// Toggle the `enabled` flag. Returns `true` if a row was updated.
    async fn set_enabled(&self, agent_id: &str, enabled: bool) -> Result<bool, DbError>;

    /// Overwrite the `behavior_policy` JSON column. Used by custom-agent
    /// upserts and catalog maintenance paths that need to persist typed
    /// adapter behavior switches. The caller is responsible for merging on
    /// top of the existing policy if partial updates are needed. Returns the
    /// updated row, or `Ok(None)` if no row matches `agent_id`.
    ///
    /// Defaulted so the many test-only stub repositories across the
    /// workspace need not implement it; the real SQLite repository
    /// overrides it. The default is intentionally an error rather than a
    /// silent no-op so a production path that forgets to override is
    /// loud, not silently broken.
    async fn set_behavior_policy(
        &self,
        agent_id: &str,
        _behavior_policy: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Err(DbError::Init(format!(
            "set_behavior_policy not implemented for this repository (agent_id '{agent_id}')"
        )))
    }

    /// Delete a row. Returns `true` if a row was removed.
    async fn delete(&self, agent_id: &str) -> Result<bool, DbError>;
}
