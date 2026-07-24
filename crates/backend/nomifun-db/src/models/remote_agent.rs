use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `remote_agents` table.
///
/// Enum-like fields (`protocol`, `auth_type`, `status`) are stored as TEXT.
/// The service layer converts them to/from `nomifun_common` enums
/// (`RemoteAgentProtocol`, `RemoteAgentAuthType`, `RemoteAgentStatus`).
///
/// Sensitive fields (`auth_token`, `device_public_key`, `device_private_key`,
/// `device_token`) are stored AES-encrypted; callers handle encryption/decryption.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RemoteAgentRow {
    pub id: i64,
    pub remote_agent_id: String,
    pub name: String,
    /// One of: "openClaw", "zeroClaw", "acp".
    pub protocol: String,
    pub url: String,
    /// One of: "bearer", "password", "none".
    pub auth_type: String,
    /// AES-encrypted authentication token.
    pub auth_token: Option<String>,
    /// Whether insecure (non-TLS) connections are allowed.
    pub allow_insecure: bool,
    pub avatar: Option<String>,
    pub description: Option<String>,
    /// OpenClaw device identifier.
    pub device_id: Option<String>,
    /// AES-encrypted Ed25519 public key.
    pub device_public_key: Option<String>,
    /// AES-encrypted Ed25519 private key.
    pub device_private_key: Option<String>,
    /// AES-encrypted device token.
    pub device_token: Option<String>,
    /// One of: "unknown", "connected", "pending", "error".
    pub status: String,
    pub last_connected_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
