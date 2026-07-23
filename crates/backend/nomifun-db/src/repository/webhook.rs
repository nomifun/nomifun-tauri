use crate::error::DbError;
use crate::models::WebhookRow;

/// Data access abstraction for the `webhooks` table.
///
/// Webhooks are global (not per-user) — they are an admin-managed pool of
/// outbound endpoints reused by features such as AutoWork completion
/// notifications.
#[async_trait::async_trait]
pub trait IWebhookRepository: Send + Sync {
    /// Insert a new webhook row. SQLite allocates the local technical ID and
    /// the repository returns the persisted row.
    async fn insert(&self, row: &WebhookRow) -> Result<WebhookRow, DbError>;

    /// Replace the mutable columns (name/platform/url/secret/description/enabled/
    /// updated_at) of an existing webhook. Returns `DbError::NotFound` if absent.
    async fn update(&self, row: &WebhookRow) -> Result<(), DbError>;

    /// Delete a webhook by its stable business ID. Returns `DbError::NotFound`
    /// if absent.
    async fn delete(&self, webhook_id: &str) -> Result<(), DbError>;

    /// Return a single webhook by its stable business ID, or `None`.
    async fn get_by_webhook_id(
        &self,
        webhook_id: &str,
    ) -> Result<Option<WebhookRow>, DbError>;

    /// Return all webhooks ordered by creation time descending (newest first).
    async fn list_all(&self) -> Result<Vec<WebhookRow>, DbError>;
}
