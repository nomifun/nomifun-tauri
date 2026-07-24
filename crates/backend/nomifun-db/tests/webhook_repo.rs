//! Integration tests for the webhook + tag_settings repositories.

use nomifun_db::models::{TagSettingRow, WebhookRow};
use nomifun_db::{
    ITagSettingRepository, IWebhookRepository, SqliteTagSettingRepository, SqliteWebhookRepository,
    init_database_memory,
};
use std::sync::Arc;

fn sample_webhook() -> WebhookRow {
    WebhookRow {
        webhook_id: nomifun_common::generate_id(),
        name: "Team bot".into(),
        platform: "lark".into(),
        url: "https://open.feishu.cn/open-apis/bot/v2/hook/abc".into(),
        secret: Some("s3cr3t".into()),
        description: "team notifications".into(),
        enabled: true,
        created_at: 1,
        updated_at: 1,
    }
}

#[tokio::test]
async fn webhook_crud_roundtrip() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IWebhookRepository> = Arc::new(SqliteWebhookRepository::new(db.pool().clone()));

    // create
    let created = repo.insert(&sample_webhook()).await.unwrap();
    assert!(nomifun_common::validate_uuidv7(&created.webhook_id).is_ok());
    // get
    let got = repo
        .get_by_webhook_id(&created.webhook_id)
        .await
        .unwrap()
        .expect("present");
    assert_eq!(got.webhook_id, created.webhook_id);
    assert_eq!(got.name, "Team bot");
    assert_eq!(got.secret.as_deref(), Some("s3cr3t"));
    // list
    let second = repo.insert(&sample_webhook()).await.unwrap();
    assert_ne!(second.webhook_id, created.webhook_id);
    let all = repo.list_all().await.unwrap();
    assert_eq!(all.len(), 2);
    // update
    let mut upd = got.clone();
    upd.name = "Renamed".into();
    upd.enabled = false;
    upd.updated_at = 9;
    repo.update(&upd).await.unwrap();
    let after = repo
        .get_by_webhook_id(&created.webhook_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.name, "Renamed");
    assert!(!after.enabled);
    // delete
    repo.delete(&created.webhook_id).await.unwrap();
    assert!(
        repo.get_by_webhook_id(&created.webhook_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn webhook_update_and_delete_missing_is_not_found() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IWebhookRepository> = Arc::new(SqliteWebhookRepository::new(db.pool().clone()));
    let missing = "0190f5fe-7c00-7a00-8000-000000000999";
    let err = repo.delete(missing).await.unwrap_err();
    assert!(matches!(err, nomifun_db::DbError::NotFound(_)));
    let mut ghost = sample_webhook();
    ghost.webhook_id = missing.to_string();
    let err = repo.update(&ghost).await.unwrap_err();
    assert!(matches!(err, nomifun_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn webhook_delete_sets_tag_setting_reference_null() {
    let db = init_database_memory().await.unwrap();
    let webhook_repo = SqliteWebhookRepository::new(db.pool().clone());
    let tag_repo = SqliteTagSettingRepository::new(db.pool().clone());
    let webhook_id = webhook_repo
        .insert(&sample_webhook())
        .await
        .unwrap()
        .webhook_id;
    tag_repo
        .upsert(&TagSettingRow {
            tag: "alpha".into(),
            webhook_id: Some(webhook_id.clone()),
            description: "bound".into(),
            notify_events: "done".into(),
            updated_at: 1,
        })
        .await
        .unwrap();

    webhook_repo.delete(&webhook_id).await.unwrap();

    let setting = tag_repo.get("alpha").await.unwrap().unwrap();
    assert_eq!(setting.webhook_id, None);
    assert_eq!(setting.description, "bound");
}

#[tokio::test]
async fn tag_setting_upsert_get_list_delete() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn ITagSettingRepository> = Arc::new(SqliteTagSettingRepository::new(db.pool().clone()));
    let webhook_repo = SqliteWebhookRepository::new(db.pool().clone());
    let webhook_id = webhook_repo
        .insert(&sample_webhook())
        .await
        .unwrap()
        .webhook_id;

    // absent → None
    assert!(repo.get("alpha").await.unwrap().is_none());

    // upsert (insert)
    repo.upsert(&TagSettingRow {
        tag: "alpha".into(),
        webhook_id: Some(webhook_id.clone()),
        description: "queue alpha".into(),
        notify_events: "done,failed,needs_review".to_string(),
        updated_at: 5,
    })
    .await
    .unwrap();
    let got = repo.get("alpha").await.unwrap().unwrap();
    assert_eq!(got.webhook_id, Some(webhook_id));

    // upsert (update — same key replaces)
    repo.upsert(&TagSettingRow {
        tag: "alpha".into(),
        webhook_id: None,
        description: "unbound now".into(),
        notify_events: "done,failed,needs_review".to_string(),
        updated_at: 6,
    })
    .await
    .unwrap();
    let got = repo.get("alpha").await.unwrap().unwrap();
    assert_eq!(got.webhook_id, None);
    assert_eq!(got.description, "unbound now");

    // list
    repo.upsert(&TagSettingRow {
        tag: "beta".into(),
        webhook_id: None,
        description: String::new(),
        notify_events: "done,failed,needs_review".to_string(),
        updated_at: 7,
    })
    .await
    .unwrap();
    assert_eq!(repo.list_all().await.unwrap().len(), 2);

    // delete (idempotent)
    repo.delete("alpha").await.unwrap();
    assert!(repo.get("alpha").await.unwrap().is_none());
    repo.delete("alpha").await.unwrap(); // no error on absent
}

#[tokio::test]
async fn webhook_repository_rejects_noncanonical_business_ids() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteWebhookRepository::new(db.pool().clone());

    for value in [
        "42",
        "550e8400-e29b-41d4-a716-446655440000",
        "0190F5FE-7C00-7A00-8000-000000000042",
        "webhook_0190f5fe-7c00-7a00-8000-000000000042",
    ] {
        let mut row = sample_webhook();
        row.webhook_id = value.to_string();
        let err = repo.insert(&row).await.unwrap_err();
        assert!(matches!(err, nomifun_db::DbError::Conflict(_)));
    }
}
