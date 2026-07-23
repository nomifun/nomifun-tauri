//! Spec §9.B wiring test: deleting a terminal session must dispatch the
//! `OnTerminalDelete` hook into `RequirementService::clear_owner_for_session`,
//! clearing the typed terminal owner column
//! of every requirement that terminal owned and re-pending any `in_progress`
//! one. This exercises the real wiring (`TerminalService::with_delete_hook` +
//! dispatch in `delete()`), not just the service method in isolation.

use std::sync::Arc;

use nomifun_api_types::{AutoWorkTargetKind, CreateRequirementRequest, RequirementStatus};
use nomifun_common::OnTerminalDelete;
use nomifun_common::{TerminalId, UserId};
use nomifun_db::{
    CreateTerminalParams, IRequirementRepository, ITerminalRepository, SqliteRequirementRepository,
    SqliteTerminalRepository, init_database_memory,
};
use nomifun_realtime::{EventBroadcaster, UserEventSink};
use nomifun_requirement::{RequirementEventEmitter, RequirementService};
use nomifun_terminal::{TerminalEventEmitter, TerminalService};

#[derive(Default)]
struct NoopBroadcaster;
impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _event: nomifun_api_types::WebSocketMessage<serde_json::Value>) {}
}
impl UserEventSink for NoopBroadcaster {
    fn send_to_user(
        &self,
        _user_id: &str,
        _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
    ) {
    }
}

#[tokio::test]
async fn deleting_terminal_clears_requirement_owner_via_hook() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let installation_owner = nomifun_db::installation_owner_id(&pool).await.unwrap();
    let term_repo: Arc<dyn ITerminalRepository> = Arc::new(SqliteTerminalRepository::new(pool.clone()));
    let req_repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(pool.clone()));

    // Requirement service is the hook target.
    let req_service = Arc::new(RequirementService::new(
        req_repo,
        RequirementEventEmitter::new(Arc::new(NoopBroadcaster), Arc::from(installation_owner.clone())),
    ));

    // Terminal service wired exactly as `nomifun-app::build_terminal_state` does:
    // register the requirement service as an `OnTerminalDelete` hook.
    let term_service = TerminalService::new(
        term_repo.clone(),
        TerminalEventEmitter::new(Arc::new(NoopBroadcaster)),
        std::env::temp_dir(),
    );
    term_service.with_delete_hook(req_service.clone() as Arc<dyn OnTerminalDelete>);

    // Persist a terminal row (no live PTY needed — delete tolerates that). The
    // terminal business UUIDv7 is returned by the repository.
    let term = term_repo
        .create(&CreateTerminalParams {
            id: TerminalId::new(),
            name: "Term One".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            command: "claude".into(),
            args: "[]".into(),
            env: None,
            backend: Some("claude".into()),
            mode: None,
            cols: 80,
            rows: 24,
            user_id: UserId::parse(installation_owner).unwrap(),
        })
        .await
        .unwrap();
    let term_id = term.terminal_id;

    // Create a requirement and let the terminal claim it (owner=term_1, in_progress).
    let r = req_service
        .create(CreateRequirementRequest {
            title: "T".into(),
            content: String::new(),
            tag: "auto".into(),
            order_key: Some("1".into()),
            status: None,
            created_by: None,
            attachments: vec![],
        })
        .await
        .unwrap();
    let claimed = req_service
        .claim_next("auto", term_id.as_str(), AutoWorkTargetKind::Terminal, 60_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.owner_terminal_id.as_deref(), Some(term_id.as_str()));
    assert!(claimed.owner_conversation_id.is_none());
    assert_eq!(claimed.status, RequirementStatus::InProgress);

    // Delete the terminal through the service → the hook fires and clears owner.
    term_service.delete(term_id.as_str()).await.unwrap();

    let after = req_service.get(&r.requirement_id).await.unwrap();
    assert_eq!(after.owner_terminal_id, None, "terminal owner cleared on delete");
    assert_eq!(after.owner_conversation_id, None);
    assert_eq!(
        after.status,
        RequirementStatus::Pending,
        "the orphaned in_progress requirement is re-pended"
    );
    assert_eq!(after.attempt_count, 1, "clearing owner must not consume an attempt");

    Box::leak(Box::new(db));
}
