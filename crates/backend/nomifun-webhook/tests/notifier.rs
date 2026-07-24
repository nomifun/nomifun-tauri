//! Tests for `CompletionNotifierImpl`: routes a finished requirement to its tag's
//! bound + enabled webhook, skips otherwise. Uses real in-memory repos + a mock sender.

use std::sync::Arc;
use std::sync::Mutex;

use nomifun_api_types::WebhookPlatform;
use nomifun_db::models::{RequirementRow, TagSettingRow, WebhookRow};
use nomifun_db::{
    ITagSettingRepository, IWebhookRepository, SqliteTagSettingRepository, SqliteWebhookRepository,
    init_database_memory,
};
use nomifun_requirement::CompletionNotifier;
use nomifun_webhook::{CompletionNotifierImpl, WebhookSender};

#[derive(Default)]
struct RecordingSender {
    calls: Mutex<Vec<Vec<(String, String)>>>, // fields per call
}

#[async_trait::async_trait]
impl WebhookSender for RecordingSender {
    async fn send_card(
        &self,
        _platform: WebhookPlatform,
        _url: &str,
        _secret: Option<&str>,
        _title: &str,
        fields: &[(String, String)],
    ) -> Result<(), nomifun_webhook::WebhookError> {
        self.calls.lock().unwrap().push(fields.to_vec());
        Ok(())
    }
}

fn requirement(tag: &str) -> RequirementRow {
    RequirementRow {
        id: 17,
        requirement_id: "0190f5fe-7c00-7a00-8000-000000000017".into(),
        display_no: 17,
        title: "Build the thing".into(),
        content: "Implement feature X".into(),
        tag: tag.into(),
        order_key: "1".into(),
        sort_seq: "00000001".into(),
        status: "done".into(),
        priority: 0,
        completion_note: Some("did it".into()),
        owner_conversation_id: None,
        owner_terminal_id: None,
        active_turn_started_at: None,
        lease_expires_at: None,
        started_at: None,
        completed_at: Some(1),
        attempt_count: 1,
        claim_generation: 0,
        claim_token: None,
        created_by: "user".into(),
        extra: "{}".into(),
        created_at: 0,
        updated_at: 1,
    }
}

struct Ctx {
    webhooks: Arc<dyn IWebhookRepository>,
    tags: Arc<dyn ITagSettingRepository>,
    sender: Arc<RecordingSender>,
}

async fn ctx() -> Ctx {
    let db = init_database_memory().await.unwrap();
    let webhooks: Arc<dyn IWebhookRepository> = Arc::new(SqliteWebhookRepository::new(db.pool().clone()));
    let tags: Arc<dyn ITagSettingRepository> = Arc::new(SqliteTagSettingRepository::new(db.pool().clone()));
    Box::leak(Box::new(db));
    Ctx {
        webhooks,
        tags,
        sender: Arc::new(RecordingSender::default()),
    }
}

async fn add_webhook(ctx: &Ctx, enabled: bool) -> String {
    ctx.webhooks
        .insert(&WebhookRow {
            webhook_id: nomifun_api_types::WebhookId::new().into_string(),
            name: "bot".into(),
            platform: "lark".into(),
            url: "https://example.com/hook".into(),
            secret: None,
            description: String::new(),
            enabled,
            created_at: 0,
            updated_at: 0,
        })
        .await
        .unwrap()
        .webhook_id
}

async fn bind_tag(ctx: &Ctx, tag: &str, webhook_id: Option<String>) {
    ctx.tags
        .upsert(&TagSettingRow {
            tag: tag.into(),
            webhook_id,
            description: String::new(),
            notify_events: "done,failed,needs_review".into(),
            updated_at: 0,
        })
        .await
        .unwrap();
}

fn notifier(ctx: &Ctx) -> CompletionNotifierImpl {
    CompletionNotifierImpl::new(ctx.tags.clone(), ctx.webhooks.clone(), ctx.sender.clone())
}

#[tokio::test]
async fn notifies_bound_enabled_webhook_with_template_fields() {
    let ctx = ctx().await;
    let wh_id = add_webhook(&ctx, true).await;
    bind_tag(&ctx, "alpha", Some(wh_id)).await;

    notifier(&ctx).notify_completion(&requirement("alpha")).await;

    let calls = ctx.sender.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "bound + enabled → one send");
    let labels: Vec<&str> = calls[0].iter().map(|(l, _)| l.as_str()).collect();
    // Template: 【需求id】【需求名】【需求内容】【完成状态】【完成记录(报告)】
    assert!(labels.contains(&"需求id"));
    assert!(labels.contains(&"需求名"));
    assert!(labels.contains(&"需求内容"));
    assert!(labels.contains(&"完成状态"));
    assert!(labels.contains(&"完成记录(报告)"));
}

#[tokio::test]
async fn skips_when_tag_unbound() {
    let ctx = ctx().await;
    add_webhook(&ctx, true).await;
    // no bind_tag → tag "alpha" has no setting
    notifier(&ctx).notify_completion(&requirement("alpha")).await;
    assert!(ctx.sender.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn skips_when_webhook_disabled() {
    let ctx = ctx().await;
    let wh_id = add_webhook(&ctx, false).await; // disabled
    bind_tag(&ctx, "alpha", Some(wh_id)).await;
    notifier(&ctx).notify_completion(&requirement("alpha")).await;
    assert!(ctx.sender.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn skips_when_binding_has_no_webhook() {
    let ctx = ctx().await;
    bind_tag(&ctx, "alpha", None).await; // setting exists but no webhook bound
    notifier(&ctx).notify_completion(&requirement("alpha")).await;
    assert!(ctx.sender.calls.lock().unwrap().is_empty());
}
