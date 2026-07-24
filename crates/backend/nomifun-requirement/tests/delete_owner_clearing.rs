//! Terminal deletion wiring must dispatch `OnTerminalDelete` into
//! `RequirementService::clear_owner_for_session`. An active claim is ambiguous,
//! so deletion parks it for review and retains its typed owner, generation,
//! capability, and start time as audit evidence. It must never requeue the work.

use std::sync::Arc;

use nomifun_api_types::{CreateRequirementRequest, RequirementStatus};
use nomifun_common::OnTerminalDelete;
use nomifun_common::{TerminalId, UserId, now_ms};
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
async fn deleting_terminal_parks_active_requirement_and_preserves_claim_evidence() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let installation_owner = nomifun_db::installation_owner_id(&pool).await.unwrap();
    let term_repo: Arc<dyn ITerminalRepository> = Arc::new(SqliteTerminalRepository::new(pool.clone()));
    let req_repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(pool.clone()));

    // Requirement service is the hook target.
    let req_service = Arc::new(RequirementService::new(
        req_repo.clone(),
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
    let claimed = req_repo
        .claim_next_for_runner("auto", None, Some(term_id.as_str()), 60_000, now_ms())
        .await
        .unwrap()
        .unwrap()
        .row;
    let claim_token = claimed.claim_token.clone().unwrap();
    assert_eq!(
        claimed.owner_terminal_id.as_deref(),
        Some(term_id.as_str())
    );
    assert!(claimed.owner_conversation_id.is_none());
    assert_eq!(claimed.status, RequirementStatus::InProgress.as_db());

    // Delete through the real service wiring. The hook parks the claim and
    // preserves its identity instead of turning it back into pending work.
    term_service.delete(term_id.as_str()).await.unwrap();

    let after = req_service.get(&r.requirement_id).await.unwrap();
    assert_eq!(
        after.owner_terminal_id.as_deref(),
        Some(term_id.as_str()),
        "the typed owner remains durable audit evidence"
    );
    assert_eq!(after.owner_conversation_id, None);
    assert_eq!(
        after.status,
        RequirementStatus::NeedsReview,
        "terminal deletion must never make ambiguous active work claimable"
    );
    assert_eq!(
        after.attempt_count, 1,
        "parking must not consume an additional attempt"
    );
    let internal = req_repo
        .get_by_requirement_id(&r.requirement_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(internal.claim_generation, claimed.claim_generation);
    assert_eq!(internal.claim_token.as_deref(), Some(claim_token.as_str()));
    assert_eq!(internal.active_turn_started_at, claimed.active_turn_started_at);
    assert_eq!(internal.lease_expires_at, None);

    Box::leak(Box::new(db));
}
