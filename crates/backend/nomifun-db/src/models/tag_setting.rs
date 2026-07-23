use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row in the `tag_settings` table — per-tag augmentation of the implicit
/// requirement tags (a bound webhook + a description). Tags themselves remain
/// derived from `requirements.tag`; this table only stores extra config keyed by
/// tag name, created on first write.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TagSettingRow {
    pub tag: String,
    /// Stable logical reference to `webhooks.webhook_id`.
    pub webhook_id: Option<String>,
    pub description: String,
    /// Comma-separated subset of `done,failed,needs_review` controlling which
    /// completion events fire the bound webhook. Defaults to all three.
    pub notify_events: String,
    pub updated_at: TimestampMs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_setting_row_roundtrips() {
        let row = TagSettingRow {
            tag: "alpha".into(),
            webhook_id: Some("0190f5fe-7c00-7a00-8000-000000000042".into()),
            description: "team alpha queue".into(),
            notify_events: "done,failed,needs_review".to_string(),
            updated_at: 9,
        };
        let json = serde_json::to_string(&row).unwrap();
        let back: TagSettingRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag, "alpha");
        assert_eq!(back.webhook_id, row.webhook_id);
    }
}
