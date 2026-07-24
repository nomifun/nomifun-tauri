use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row in the `webhooks` table — a reusable outbound webhook endpoint.
///
/// The SQLite-local technical key is intentionally omitted. `webhook_id` is the
/// stable UUIDv7 business identity used by every relationship and API outside
/// the repository.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WebhookRow {
    pub webhook_id: String,
    pub name: String,
    /// Platform discriminator; `lark` is the only supported value in v1.
    pub platform: String,
    pub url: String,
    /// Optional signing secret (Lark "加签"); never returned to clients.
    pub secret: Option<String>,
    pub description: String,
    pub enabled: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_row_roundtrips() {
        let row = WebhookRow {
            webhook_id: "0190f5fe-7c00-7a00-8000-000000000041".into(),
            name: "Team bot".into(),
            platform: "lark".into(),
            url: "https://open.feishu.cn/open-apis/bot/v2/hook/xxx".into(),
            secret: Some("s3cr3t".into()),
            description: "notifications".into(),
            enabled: true,
            created_at: 1,
            updated_at: 2,
        };
        let json = serde_json::to_string(&row).unwrap();
        let back: WebhookRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.webhook_id, row.webhook_id);
        assert_eq!(back.platform, "lark");
        assert!(back.enabled);
    }
}
