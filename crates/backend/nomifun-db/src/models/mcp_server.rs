use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `mcp_servers` table.
///
/// Enum-like fields (`transport_type`, `status`) are stored as TEXT.
/// The service layer converts them to/from domain enums.
///
/// JSON fields (`transport_config`, `tools`) are stored as TEXT in SQLite
/// and deserialized by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct McpServerRow {
    /// Stable MCP server business identifier (canonical bare lowercase UUIDv7).
    ///
    /// The table's integer `id` is a repository-private technical key and is
    /// deliberately not represented by this row model.
    pub mcp_server_id: String,
    /// Unique server name (used as identifier when syncing to Agent CLIs).
    pub name: String,
    pub description: Option<String>,
    /// Whether this server is synced to Agent CLIs.
    pub enabled: bool,
    /// One of: "stdio", "sse", "http".
    pub transport_type: String,
    /// JSON object: command/args/env (stdio) or url/headers (sse/http).
    pub transport_config: String,
    /// JSON array of tool descriptions (populated after connection test).
    pub tools: Option<String>,
    /// One of: "connected", "disconnected", "error", "testing".
    /// Represents the latest test result, not a live runtime state.
    pub last_test_status: String,
    pub last_connected: Option<TimestampMs>,
    /// Original JSON text for editing restoration.
    pub original_json: Option<String>,
    /// Whether this is a built-in server (hidden from edit/delete in UI).
    pub builtin: bool,
    /// Soft-delete timestamp. `NULL` means active.
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
