//! Verifies `RequirementService::set_status` fires the `CompletionNotifier`
//! exactly once on a terminal transition, and not on no-op / non-terminal ones.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use nomifun_api_types::{CreateRequirementRequest, RequirementStatus};
use nomifun_db::models::RequirementRow;
use nomifun_db::{IRequirementRepository, SqliteRequirementRepository, init_database_memory};
use nomifun_realtime::UserEventSink;
use nomifun_requirement::{CompletionNotifier, RequirementEventEmitter, RequirementService};

#[derive(Default)]
struct NoopBroadcaster;
impl UserEventSink for NoopBroadcaster {
    fn send_to_user(
        &self,
        _user_id: &str,
        _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
    ) {
    }
}

#[derive(Default)]
struct RecordingNotifier {
    ids: Mutex<Vec<String>>,
}

#[async_trait]
impl CompletionNotifier for RecordingNotifier {
    async fn notify_completion(&self, requirement: &RequirementRow) {
        self.ids
            .lock()
            .unwrap()
            .push(requirement.requirement_id.clone());
    }
}

async fn svc(notifier: Arc<RecordingNotifier>) -> RequirementService {
    let db = init_database_memory().await.unwrap();
    let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
    let repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
    let emitter = RequirementEventEmitter::new(
        Arc::new(NoopBroadcaster),
        Arc::from(installation_owner.as_str()),
    );
    let service = RequirementService::new(repo, emitter)
        .with_completion_notifier(notifier);
    Box::leak(Box::new(db));
    service
}

fn new_req(tag: &str) -> CreateRequirementRequest {
    CreateRequirementRequest {
        title: "T".into(),
        content: "body".into(),
        tag: tag.into(),
        order_key: Some("1".into()),
        status: None,
        created_by: None,
        attachments: vec![],
    }
}

/// Let the detached `tokio::spawn`(notify) task run.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn fires_once_on_done() {
    let notifier = Arc::new(RecordingNotifier::default());
    let s = svc(notifier.clone()).await;
    let r = s.create(new_req("alpha")).await.unwrap();

    s.set_status(&r.requirement_id, RequirementStatus::Done, Some("note".into()))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        notifier.ids.lock().unwrap().as_slice(),
        std::slice::from_ref(&r.requirement_id)
    );

    // Re-setting the same terminal status is an idempotent no-op → no extra fire.
    let _ = s
        .set_status(&r.requirement_id, RequirementStatus::Done, None)
        .await
        .unwrap();
    settle().await;
    assert_eq!(notifier.ids.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fires_on_failed() {
    let notifier = Arc::new(RecordingNotifier::default());
    let s = svc(notifier.clone()).await;
    let r = s.create(new_req("alpha")).await.unwrap();
    s.set_status(
        &r.requirement_id,
        RequirementStatus::Failed,
        Some("oops".into()),
    )
        .await
        .unwrap();
    settle().await;
    assert_eq!(notifier.ids.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn does_not_fire_on_non_terminal() {
    let notifier = Arc::new(RecordingNotifier::default());
    let s = svc(notifier.clone()).await;
    let r = s.create(new_req("alpha")).await.unwrap();
    s.set_status(&r.requirement_id, RequirementStatus::InProgress, None)
        .await
        .unwrap();
    settle().await;
    assert!(notifier.ids.lock().unwrap().is_empty());
}
