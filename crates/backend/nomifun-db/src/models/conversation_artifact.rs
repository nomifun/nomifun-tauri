use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `conversation_artifacts` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConversationArtifactRow {
    /// Stable product identity. The table's AUTOINCREMENT `id` is deliberately
    /// not mapped outside the SQLite repository.
    pub conversation_artifact_id: String,
    pub conversation_id: String,
    pub cron_job_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub payload: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
