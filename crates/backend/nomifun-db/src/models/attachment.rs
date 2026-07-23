use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row in the `attachments` table — requirement images.
///
/// Both the file and its requirement use stable UUIDv7 business identities.
/// The local technical row IDs never cross this model's logical relationship.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AttachmentRow {
    pub id: i64,
    pub attachment_id: String,
    pub requirement_id: String,
    /// Original display name, deduped per requirement (`name(2).ext`).
    pub file_name: String,
    /// Path relative to the data dir, e.g.
    /// `attachments/{requirement_id}/{attachment_id}.png`.
    /// Stored relative so desktop data-dir relocation never has to rewrite it.
    pub rel_path: String,
    pub mime: String,
    pub size_bytes: i64,
    pub created_by: Option<String>,
    pub created_at: TimestampMs,
}
