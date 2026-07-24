use nomifun_common::validate_uuidv7;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::WebhookRow;
use crate::repository::webhook::IWebhookRepository;

#[derive(Clone, Debug)]
pub struct SqliteWebhookRepository {
    pool: SqlitePool,
}

impl SqliteWebhookRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IWebhookRepository for SqliteWebhookRepository {
    async fn insert(&self, row: &WebhookRow) -> Result<WebhookRow, DbError> {
        validate_uuidv7(&row.webhook_id).map_err(|error| {
            DbError::Conflict(format!("invalid webhook_id '{}': {error}", row.webhook_id))
        })?;
        let inserted = sqlx::query_as::<_, WebhookRow>(
            "INSERT INTO webhooks (\
                webhook_id, name, platform, url, secret, description, enabled, created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
            RETURNING webhook_id, name, platform, url, secret, description, enabled, created_at, updated_at",
        )
        .bind(&row.webhook_id)
        .bind(&row.name)
        .bind(&row.platform)
        .bind(&row.url)
        .bind(&row.secret)
        .bind(&row.description)
        .bind(row.enabled)
        .bind(row.created_at)
        .bind(row.updated_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(inserted)
    }

    async fn update(&self, row: &WebhookRow) -> Result<(), DbError> {
        validate_uuidv7(&row.webhook_id).map_err(|error| {
            DbError::Conflict(format!("invalid webhook_id '{}': {error}", row.webhook_id))
        })?;
        let result = sqlx::query(
            "UPDATE webhooks SET \
                name = ?, platform = ?, url = ?, secret = ?, description = ?, enabled = ?, updated_at = ? \
             WHERE webhook_id = ?",
        )
        .bind(&row.name)
        .bind(&row.platform)
        .bind(&row.url)
        .bind(&row.secret)
        .bind(&row.description)
        .bind(row.enabled)
        .bind(row.updated_at)
        .bind(&row.webhook_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("webhook {}", row.webhook_id)));
        }
        Ok(())
    }

    async fn delete(&self, webhook_id: &str) -> Result<(), DbError> {
        validate_uuidv7(webhook_id).map_err(|error| {
            DbError::Conflict(format!("invalid webhook_id '{webhook_id}': {error}"))
        })?;
        let mut transaction = self.pool.begin().await?;
        let locked =
            sqlx::query("UPDATE webhooks SET updated_at = updated_at WHERE webhook_id = ?")
                .bind(webhook_id)
                .execute(&mut *transaction)
                .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("webhook {webhook_id}")));
        }

        // SET_NULL: tag notification configuration survives, but becomes
        // explicitly unbound from the deleted endpoint.
        sqlx::query("UPDATE tag_settings SET webhook_id = NULL WHERE webhook_id = ?")
            .bind(webhook_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM webhooks WHERE webhook_id = ?")
            .bind(webhook_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn get_by_webhook_id(
        &self,
        webhook_id: &str,
    ) -> Result<Option<WebhookRow>, DbError> {
        validate_uuidv7(webhook_id).map_err(|error| {
            DbError::Conflict(format!("invalid webhook_id '{webhook_id}': {error}"))
        })?;
        let row = sqlx::query_as::<_, WebhookRow>(
            "SELECT webhook_id, name, platform, url, secret, description, enabled, created_at, updated_at \
             FROM webhooks WHERE webhook_id = ?",
        )
        .bind(webhook_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_all(&self) -> Result<Vec<WebhookRow>, DbError> {
        let rows = sqlx::query_as::<_, WebhookRow>(
            "SELECT webhook_id, name, platform, url, secret, description, enabled, created_at, updated_at \
             FROM webhooks ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
