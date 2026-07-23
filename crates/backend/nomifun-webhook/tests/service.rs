//! Integration tests for `WebhookService` (CRUD + tag settings + test) using a
//! real in-memory DB and a recording mock sender.

use std::sync::Arc;
use std::sync::Mutex;

use nomifun_api_types::{CreateWebhookRequest, UpdateWebhookRequest, UpsertTagSettingRequest, WebhookPlatform};
use nomifun_db::{
    ITagSettingRepository, IWebhookRepository, SqliteTagSettingRepository, SqliteWebhookRepository,
    init_database_memory,
};
use nomifun_webhook::{WebhookSender, WebhookService};

#[derive(Default)]
struct MockSender {
    calls: Mutex<Vec<(String, String)>>, // (url, title)
    fail: bool,
}

#[async_trait::async_trait]
impl WebhookSender for MockSender {
    async fn send_card(
        &self,
        _platform: WebhookPlatform,
        url: &str,
        _secret: Option<&str>,
        title: &str,
        _fields: &[(String, String)],
    ) -> Result<(), nomifun_webhook::WebhookError> {
        self.calls.lock().unwrap().push((url.to_string(), title.to_string()));
        if self.fail {
            return Err(nomifun_webhook::WebhookError::Remote("boom".into()));
        }
        Ok(())
    }
}

async fn svc(sender: Arc<dyn WebhookSender>) -> WebhookService {
    let db = init_database_memory().await.unwrap();
    let webhooks: Arc<dyn IWebhookRepository> = Arc::new(SqliteWebhookRepository::new(db.pool().clone()));
    let tags: Arc<dyn ITagSettingRepository> = Arc::new(SqliteTagSettingRepository::new(db.pool().clone()));
    Box::leak(Box::new(db));
    WebhookService::new(webhooks, tags, sender)
}

fn create_req() -> CreateWebhookRequest {
    CreateWebhookRequest {
        name: "Team bot".into(),
        url: "https://open.feishu.cn/open-apis/bot/v2/hook/abc".into(),
        platform: WebhookPlatform::Lark,
        description: "notify".into(),
        secret: Some("s3cr3t".into()),
        enabled: Some(true),
    }
}

#[tokio::test]
async fn create_list_update_delete_and_secret_is_hidden() {
    let s = svc(Arc::new(MockSender::default())).await;

    let created = s.create(create_req()).await.unwrap();
    assert_eq!(created.name, "Team bot");
    // secret must never be echoed; has_secret signals presence.
    assert!(created.has_secret);
    assert!(nomifun_api_types::WebhookId::parse(created.webhook_id.as_str()).is_ok());

    let list = s.list().await.unwrap();
    assert_eq!(list.len(), 1);

    let updated = s
        .update(
            &created.webhook_id,
            UpdateWebhookRequest {
                name: Some("Renamed".into()),
                enabled: Some(false),
                secret: Some(None), // clear the secret
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "Renamed");
    assert!(!updated.enabled);
    assert!(!updated.has_secret, "secret cleared via Some(None)");

    s.delete(&created.webhook_id).await.unwrap();
    assert!(s.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn create_validates_name_and_url() {
    let s = svc(Arc::new(MockSender::default())).await;
    let mut bad = create_req();
    bad.name = "  ".into();
    assert!(s.create(bad).await.is_err());
    let mut bad = create_req();
    bad.url = "".into();
    assert!(s.create(bad).await.is_err());
}

#[tokio::test]
async fn test_sends_card_and_propagates_failure() {
    // success
    let ok_sender = Arc::new(MockSender::default());
    let s = svc(ok_sender.clone()).await;
    let wh = s.create(create_req()).await.unwrap();
    s.test(&wh.webhook_id).await.unwrap();
    assert_eq!(ok_sender.calls.lock().unwrap().len(), 1);

    // failure → BadGateway
    let fail_sender = Arc::new(MockSender {
        fail: true,
        ..Default::default()
    });
    let s2 = svc(fail_sender).await;
    let wh2 = s2.create(create_req()).await.unwrap();
    let err = s2.test(&wh2.webhook_id).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::BadGateway(_)));
}

#[tokio::test]
async fn tag_setting_upsert_validates_webhook_exists() {
    let s = svc(Arc::new(MockSender::default())).await;
    // binding a non-existent webhook → BadRequest
    let err = s
        .upsert_tag_setting(
            "alpha",
            UpsertTagSettingRequest {
                webhook_id: Some(Some(
                    nomifun_api_types::WebhookId::parse(
                        "0190f5fe-7c00-7a00-8000-000000000999",
                    )
                    .unwrap(),
                )),
                description: Some("x".into()),
                notify_events: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::BadRequest(_)));

    // create a webhook, then bind it
    let wh = s.create(create_req()).await.unwrap();
    let setting = s
        .upsert_tag_setting(
            "alpha",
            UpsertTagSettingRequest {
                webhook_id: Some(Some(wh.webhook_id.clone())),
                description: Some("queue alpha".into()),
                notify_events: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(setting.webhook_id, Some(wh.webhook_id.clone()));
    assert_eq!(setting.description, "queue alpha");

    // get an unset tag → empty default shape
    let empty = s.get_tag_setting("never-set").await.unwrap();
    assert_eq!(empty.tag, "never-set");
    assert!(empty.webhook_id.is_none());

    // partial update keeps webhook binding when only description changes
    let only_desc = s
        .upsert_tag_setting(
            "alpha",
            UpsertTagSettingRequest {
                webhook_id: None,
                description: Some("changed".into()),
                notify_events: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(only_desc.webhook_id, Some(wh.webhook_id.clone()));
    assert_eq!(only_desc.description, "changed");

    // clear the binding via Some(None)
    let cleared = s
        .upsert_tag_setting(
            "alpha",
            UpsertTagSettingRequest {
                webhook_id: Some(None),
                description: None,
                notify_events: None,
            },
        )
        .await
        .unwrap();
    assert!(cleared.webhook_id.is_none());
}
