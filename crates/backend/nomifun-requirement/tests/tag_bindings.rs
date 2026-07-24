//! Tests for `RequirementService::tag_bindings`: enumerates conversation +
//! terminal AutoWork bindings, grouped by tag, only for enabled ones.

use std::sync::Arc;

use nomifun_db::models::ConversationRow;
use nomifun_db::{
    CreateTerminalParams, IConversationRepository, IRequirementRepository, ITerminalRepository,
    SqliteConversationRepository, SqliteRequirementRepository, SqliteTerminalRepository, init_database_memory,
};
use nomifun_realtime::UserEventSink;
use nomifun_requirement::{RequirementEventEmitter, RequirementService};
use nomifun_common::{ConversationId, TerminalId, UserId};

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

fn conv(name: &str, autowork_json: &str) -> ConversationRow {
    ConversationRow {
        id: 0,
        conversation_id: ConversationId::new().into_string(),
        user_id: String::new(),
        name: name.into(),
        r#type: "nomi".into(),
        extra: autowork_json.into(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: None,
        status: Some("pending".into()),
        source: None,
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        created_at: 0,
        updated_at: 0,
    }
}

#[tokio::test]
async fn groups_enabled_conversation_and_terminal_bindings_by_tag() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let installation_owner = nomifun_db::installation_owner_id(&pool).await.unwrap();
    let typed_installation_owner = UserId::parse(&installation_owner).unwrap();
    let conv_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let term_repo: Arc<dyn ITerminalRepository> = Arc::new(SqliteTerminalRepository::new(pool.clone()));
    let req_repo: Arc<dyn IRequirementRepository> = Arc::new(SqliteRequirementRepository::new(pool.clone()));

    // Two conversations enabled on tag "x", one disabled, one with no autowork.
    // The technical row id is not part of the binding contract.
    let mut c = conv("Alpha A", r#"{"autowork":{"enabled":true,"tag":"x"}}"#);
    c.user_id = installation_owner.clone();
    conv_repo.create(&c)
        .await
        .unwrap();
    let mut c = conv("Alpha B", r#"{"autowork":{"enabled":true,"tag":"x"}}"#);
    c.user_id = installation_owner.clone();
    conv_repo.create(&c)
        .await
        .unwrap();
    let mut c = conv("Disabled", r#"{"autowork":{"enabled":false,"tag":"x"}}"#);
    c.user_id = installation_owner.clone();
    conv_repo.create(&c)
        .await
        .unwrap();
    let mut c = conv("No autowork", "{}");
    c.user_id = installation_owner.clone();
    conv_repo.create(&c).await.unwrap();

    // One terminal enabled on tag "y"; its business UUIDv7 is returned.
    let term = term_repo
        .create(&CreateTerminalParams {
            id: TerminalId::new(),
            name: "Term One".into(),
            cwd: "/tmp".into(),
            command: "claude".into(),
            args: "[]".into(),
            env: None,
            backend: Some("claude".into()),
            mode: None,
            cols: 80,
            rows: 24,
            user_id: typed_installation_owner,
        })
        .await
        .unwrap();
    let term_id = term.terminal_id;
    term_repo
        .update_autowork(term_id.as_str(), Some(r#"{"enabled":true,"tag":"y"}"#))
        .await
        .unwrap();

    let svc = RequirementService::new(
        req_repo,
        RequirementEventEmitter::new(Arc::new(NoopBroadcaster), Arc::from(installation_owner.clone())),
    )
        .with_conversation_repo(conv_repo)
        .with_terminal_repo(term_repo);
    Box::leak(Box::new(db));

    let groups = svc.tag_bindings(&installation_owner).await.unwrap();

    // tag "x" has the two enabled conversations; "y" has the terminal. Disabled +
    // no-autowork conversations are excluded.
    let x = groups.iter().find(|g| g.tag == "x").expect("tag x present");
    assert_eq!(x.bindings.len(), 2);
    let mut names: Vec<&str> = x.bindings.iter().map(|b| b.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["Alpha A", "Alpha B"]);

    let y = groups.iter().find(|g| g.tag == "y").expect("tag y present");
    assert_eq!(y.bindings.len(), 1);
    assert_eq!(y.bindings[0].target_id, term_id.to_string());

    // No "active" run_state without a live AutoWork runner (route enriches that).
    assert!(
        groups
            .iter()
            .flat_map(|g| &g.bindings)
            .all(|b| b.run_state == nomifun_api_types::AutoWorkRunState::Idle)
    );
}
