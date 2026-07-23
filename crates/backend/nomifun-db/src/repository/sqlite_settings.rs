use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::SystemSettings;
use crate::repository::ISettingsRepository;

/// SQLite-backed implementation of [`ISettingsRepository`].
#[derive(Clone, Debug)]
pub struct SqliteSettingsRepository {
    pool: SqlitePool,
}

impl SqliteSettingsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ISettingsRepository for SqliteSettingsRepository {
    async fn get_settings(&self) -> Result<Option<SystemSettings>, DbError> {
        let row = sqlx::query_as::<_, SystemSettings>(
            "SELECT * FROM system_settings WHERE singleton_key = 'system'",
        )
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn upsert_settings(
        &self,
        language: &str,
        notification_enabled: bool,
        cron_notification_enabled: bool,
        command_queue_enabled: bool,
        save_upload_to_workspace: bool,
    ) -> Result<SystemSettings, DbError> {
        let now = nomifun_common::now_ms();

        sqlx::query(
            "INSERT INTO system_settings \
                (singleton_key, language, notification_enabled, cron_notification_enabled, \
                 command_queue_enabled, save_upload_to_workspace, updated_at) \
             VALUES ('system', ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(singleton_key) DO UPDATE SET \
                language = excluded.language, \
                notification_enabled = excluded.notification_enabled, \
                cron_notification_enabled = excluded.cron_notification_enabled, \
                command_queue_enabled = excluded.command_queue_enabled, \
                save_upload_to_workspace = excluded.save_upload_to_workspace, \
                updated_at = excluded.updated_at",
        )
        .bind(language)
        .bind(notification_enabled)
        .bind(cron_notification_enabled)
        .bind(command_queue_enabled)
        .bind(save_upload_to_workspace)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(SystemSettings {
            id: sqlx::query_scalar(
                "SELECT id FROM system_settings WHERE singleton_key = 'system'",
            )
            .fetch_one(&self.pool)
            .await?,
            singleton_key: "system".into(),
            language: language.to_string(),
            notification_enabled,
            cron_notification_enabled,
            command_queue_enabled,
            save_upload_to_workspace,
            updated_at: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteSettingsRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteSettingsRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn get_settings_returns_v3_baseline_defaults() {
        let (repo, _db) = setup().await;
        let settings = repo.get_settings().await.unwrap().unwrap();
        assert_eq!(settings.singleton_key, "system");
        assert_eq!(settings.language, "en-US");
        assert!(settings.notification_enabled);
        assert!(!settings.cron_notification_enabled);
        assert!(!settings.command_queue_enabled);
        assert!(!settings.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn upsert_creates_settings() {
        let (repo, _db) = setup().await;
        let s = repo.upsert_settings("zh-CN", false, true, true, false).await.unwrap();

        assert_eq!(s.id, 1);
        assert_eq!(s.language, "zh-CN");
        assert!(!s.notification_enabled);
        assert!(s.cron_notification_enabled);
        assert!(s.command_queue_enabled);
        assert!(!s.save_upload_to_workspace);
        assert!(s.updated_at > 0);
    }

    #[tokio::test]
    async fn upsert_then_get_returns_same() {
        let (repo, _db) = setup().await;
        repo.upsert_settings("en-US", true, false, false, true).await.unwrap();

        let s = repo.get_settings().await.unwrap().unwrap();
        assert_eq!(s.language, "en-US");
        assert!(s.notification_enabled);
        assert!(!s.cron_notification_enabled);
        assert!(!s.command_queue_enabled);
        assert!(s.save_upload_to_workspace);
    }

    #[tokio::test]
    async fn upsert_overwrites_existing() {
        let (repo, _db) = setup().await;
        repo.upsert_settings("en-US", true, false, false, false).await.unwrap();
        let s = repo.upsert_settings("zh-CN", false, true, true, true).await.unwrap();

        assert_eq!(s.language, "zh-CN");
        assert!(!s.notification_enabled);
        assert!(s.cron_notification_enabled);
        assert!(s.command_queue_enabled);
        assert!(s.save_upload_to_workspace);

        // Verify persisted via get
        let fetched = repo.get_settings().await.unwrap().unwrap();
        assert_eq!(fetched.language, "zh-CN");
    }
}
