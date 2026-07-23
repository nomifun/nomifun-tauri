use std::sync::Arc;

use nomifun_ai_agent::{
    AgentStreamEvent,
    protocol::events::{
        FinishEventData, ToolCallEventData, ToolCallRetryData, ToolCallStatus,
    },
};
use nomifun_common::{ConversationId, MessageId, now_ms};
use nomifun_conversation::stream_relay::StreamRelay;
use nomifun_db::{
    IConversationRepository, SortOrder, SqliteConversationRepository, init_database_memory,
    models::{ConversationRow, MessageRow},
};
use nomifun_realtime::WebSocketManager;
use serde_json::json;
use tokio::sync::broadcast;

async fn setup_repo() -> (Arc<SqliteConversationRepository>, nomifun_db::Database, String, String) {
    let db = init_database_memory().await.unwrap();
    let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let now = now_ms();
    let conversation_id = ConversationId::new().into_string();
    repo.create(&ConversationRow {
        id: 0,
        conversation_id: conversation_id.clone(),
        user_id: installation_owner.clone(),
        name: "Tool call test".into(),
        r#type: "nomi".into(),
        extra: "{}".into(),
        delegation_policy: "automatic".into(),
        execution_model_pool: None,
        decision_policy: "automatic".into(),
        execution_template_id: None,
        model: None,
        status: Some("running".into()),
        source: Some("nomifun".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();

    (repo, db, conversation_id, installation_owner)
}

#[tokio::test]
async fn run_tool_call_with_empty_call_id_is_not_persisted() {
    let (repo, _db, conversation_id, installation_owner) = setup_repo().await;
    let bus = Arc::new(WebSocketManager::new());
    let (tx, _) = broadcast::channel(64);

    let relay = StreamRelay::new(
        conversation_id.clone(),
        "asst-1".into(),
        installation_owner.into(),
        repo.clone(),
        bus,
        None,
    );

    let rx = tx.subscribe();
    tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id: "".into(),
        name: "Glob".into(),
        args: json!({"pattern": "*.rs"}),
        status: ToolCallStatus::Running,
        input: Some(json!({"pattern": "*.rs"})),
        output: None,
        description: None,
        artifacts: Vec::new(),
        retry: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

    relay.consume(rx).await;

    let messages = repo.get_messages(&conversation_id, 1, 100, SortOrder::Asc).await.unwrap();

    assert!(
        messages.items.iter().all(|row| row.r#type != "tool_call"),
        "empty call_id tool_call must not be persisted"
    );
}

#[tokio::test]
async fn retry_identity_is_persisted_with_the_tool_call_receipt() {
    let (repo, _db, conversation_id, installation_owner) = setup_repo().await;
    let bus = Arc::new(WebSocketManager::new());
    let (tx, _) = broadcast::channel(64);
    let root_turn_id = MessageId::new().into_string();
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: root_turn_id.clone(),
        conversation_id: conversation_id.clone(),
        msg_id: Some(root_turn_id.clone()),
        r#type: "system".into(),
        content: r#"{"kind":"turn_root"}"#.into(),
        position: Some("center".into()),
        status: Some("finish".into()),
        hidden: true,
        created_at: now_ms(),
    })
    .await
    .unwrap();
    let relay = StreamRelay::new(
        conversation_id.clone(),
        root_turn_id,
        installation_owner.into(),
        repo.clone(),
        bus,
        None,
    );

    let rx = tx.subscribe();
    tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
        call_id: "nomi-call-2".into(),
        name: "nomi_delegate".into(),
        args: json!({"strategy": "parallel"}),
        status: ToolCallStatus::Completed,
        input: Some(json!({"strategy": "parallel"})),
        output: Some("execution created".into()),
        description: None,
        retry: Some(ToolCallRetryData {
            retry_group_id: "nomi-call-1".into(),
            attempt_no: 2,
            retry_of_call_id: Some("nomi-call-1".into()),
        }),
        artifacts: Vec::new(),
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
        .unwrap();

    relay.consume(rx).await;

    let messages = repo
        .get_messages(&conversation_id, 1, 100, SortOrder::Asc)
        .await
        .unwrap();
    let receipt = messages
        .items
        .iter()
        .find(|row| row.r#type == "tool_call")
        .expect("tool receipt must be persisted");
    let content: serde_json::Value = serde_json::from_str(&receipt.content).unwrap();
    assert_eq!(content["retry"]["retry_group_id"], "nomi-call-1");
    assert_eq!(content["retry"]["attempt_no"], 2);
    assert_eq!(content["retry"]["retry_of_call_id"], "nomi-call-1");
}
