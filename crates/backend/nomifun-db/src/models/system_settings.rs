use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `system_settings` table.
///
/// Logical singleton selected by `singleton_key`; `id` is not hard-coded.
/// Boolean fields are stored as INTEGER in SQLite (0/1) and mapped to `bool`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SystemSettings {
    pub id: i64,
    pub singleton_key: String,
    pub language: String,
    pub notification_enabled: bool,
    pub cron_notification_enabled: bool,
    pub command_queue_enabled: bool,
    pub save_upload_to_workspace: bool,
    pub updated_at: TimestampMs,
}
