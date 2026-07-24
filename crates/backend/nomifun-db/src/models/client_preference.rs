use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `client_preferences` table.
///
/// Generic key-value store. Values are stored as JSON-serialized TEXT.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ClientPreference {
    pub id: i64,
    pub key: String,
    pub value: String,
    pub updated_at: TimestampMs,
}
