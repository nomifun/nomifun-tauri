use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `acp_session` table.
///
/// Stores ACP agent session state for suspend/resume across app restarts.
/// `conversation_id` is the logical owner key (one session per conversation);
/// `id` is the local technical row identity.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AcpSessionRow {
    pub id: i64,
    pub conversation_id: String,
    pub agent_backend: String,
    pub agent_source: String,
    pub agent_id: String,
    /// ACP protocol session locator. This is external/opaque, not a NomiFun ID.
    pub acp_session_id: Option<String>,
    pub session_status: String,
    /// JSON object: serialized session configuration.
    pub session_config: String,
    pub last_active_at: Option<TimestampMs>,
    pub suspended_at: Option<TimestampMs>,
}
