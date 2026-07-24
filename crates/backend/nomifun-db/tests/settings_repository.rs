//! Black-box integration tests for ISettingsRepository.
//!
//! Tests exercise the public trait interface against an in-memory SQLite database.

use std::sync::Arc;

use nomifun_db::{ISettingsRepository, SqliteSettingsRepository, init_database_memory};

async fn repo() -> Arc<dyn ISettingsRepository> {
    let db = init_database_memory().await.unwrap();
    Arc::new(SqliteSettingsRepository::new(db.pool().clone()))
}

// -- Get baseline default state --

#[tokio::test]
async fn get_settings_returns_v3_baseline_defaults() {
    let r = repo().await;
    let settings = r
        .get_settings()
        .await
        .unwrap()
        .expect("v3 baseline seeds the settings singleton");
    assert_eq!(settings.language, "en-US");
    assert!(settings.notification_enabled);
    assert!(!settings.cron_notification_enabled);
    assert!(!settings.command_queue_enabled);
    assert!(!settings.save_upload_to_workspace);
}

// -- Upsert creates a row --

#[tokio::test]
async fn upsert_creates_settings_with_given_values() {
    let r = repo().await;
    let s = r.upsert_settings("zh-CN", false, true, true, false).await.unwrap();

    assert_eq!(s.language, "zh-CN");
    assert!(!s.notification_enabled);
    assert!(s.cron_notification_enabled);
    assert!(s.command_queue_enabled);
    assert!(!s.save_upload_to_workspace);
    assert!(s.updated_at > 0);
}

// -- Upsert then get round-trip --

#[tokio::test]
async fn upsert_then_get_returns_consistent_data() {
    let r = repo().await;
    r.upsert_settings("en-US", true, false, false, true).await.unwrap();

    let s = r.get_settings().await.unwrap().unwrap();
    assert_eq!(s.language, "en-US");
    assert!(s.notification_enabled);
    assert!(!s.cron_notification_enabled);
    assert!(!s.command_queue_enabled);
    assert!(s.save_upload_to_workspace);
}

// -- Upsert overwrites --

#[tokio::test]
async fn upsert_overwrites_previous_settings() {
    let r = repo().await;
    r.upsert_settings("en-US", true, false, false, false).await.unwrap();
    r.upsert_settings("zh-CN", false, true, true, true).await.unwrap();

    let s = r.get_settings().await.unwrap().unwrap();
    assert_eq!(s.language, "zh-CN");
    assert!(!s.notification_enabled);
    assert!(s.cron_notification_enabled);
    assert!(s.command_queue_enabled);
    assert!(s.save_upload_to_workspace);
}

// -- updated_at advances on each upsert --

#[tokio::test]
async fn upsert_advances_updated_at() {
    let r = repo().await;
    let first = r.upsert_settings("en-US", true, false, false, false).await.unwrap();
    let second = r.upsert_settings("en-US", true, false, false, false).await.unwrap();

    assert!(second.updated_at >= first.updated_at);
}
