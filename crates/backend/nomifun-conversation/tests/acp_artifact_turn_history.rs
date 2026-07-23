use std::path::{Path, PathBuf};
use std::sync::Arc;

use nomifun_ai_agent::{
    AgentStreamEvent,
    artifact_store::{ArtifactKind, ArtifactStore, PersistedArtifact},
    protocol::events::{
        AcpToolCallContentItem, FinishEventData, TextEventData, TurnStopReason,
        tool_call::{
            AcpToolCallEventData, AcpToolCallKind, AcpToolCallSessionUpdateKind,
            AcpToolCallStatus, AcpToolCallTextBlock, AcpToolCallTextBlockType,
            AcpToolCallUpdateData,
        },
    },
};
use nomifun_common::{ConversationId, MessageId, now_ms};
use nomifun_conversation::stream_relay::{RelayTerminal, StreamRelay};
use nomifun_db::{
    IConversationRepository, SortOrder, SqliteConversationRepository, init_database_memory,
    models::{ConversationRow, MessageRow},
};
use nomifun_realtime::BroadcastEventBus;
use serde_json::{Value, json};
use tokio::sync::broadcast;

const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

struct TestWorkspace(PathBuf);

impl TestWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nomifun-acp-artifact-history-{}",
            MessageId::new().into_string()
        ));
        std::fs::create_dir_all(&path).expect("create artifact test workspace");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn setup_repo() -> (
    Arc<SqliteConversationRepository>,
    nomifun_db::Database,
    String,
    String,
) {
    let db = init_database_memory().await.expect("initialize SQLite test database");
    let owner = nomifun_db::installation_owner_id(db.pool())
        .await
        .expect("resolve installation owner");
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let conversation_id = ConversationId::new().into_string();
    let now = now_ms();
    repo.create(&ConversationRow {
        id: 0,
        conversation_id: conversation_id.clone(),
        user_id: owner.clone(),
        name: "ACP artifact turn history".into(),
        r#type: "acp".into(),
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
    .expect("create conversation");
    (repo, db, conversation_id, owner)
}

fn artifact_items(content: &Value) -> Vec<PersistedArtifact> {
    content["update"]["content"]
        .as_array()
        .expect("ACP content array")
        .iter()
        .filter(|item| item["type"] == "artifact")
        .map(|item| {
            serde_json::from_value(item["artifact"].clone())
                .expect("deserialize persisted artifact receipt")
        })
        .collect()
}

async fn insert_turn_parent(
    repo: &SqliteConversationRepository,
    conversation_id: &str,
    turn_id: &str,
) {
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: turn_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        msg_id: Some(turn_id.to_owned()),
        r#type: "system".to_owned(),
        content: r#"{"kind":"turn_root"}"#.to_owned(),
        position: Some("center".to_owned()),
        status: Some("finish".to_owned()),
        hidden: true,
        created_at: now_ms(),
    })
    .await
    .expect("insert logical root turn message");
}

#[tokio::test]
async fn sparse_acp_completion_commits_to_the_root_turn_and_hydrates_as_finished() {
    let (repo, _db, conversation_id, owner) = setup_repo().await;
    let workspace = TestWorkspace::new();
    let store = ArtifactStore::new(workspace.path());
    let artifact = store
        .persist_inline(ArtifactKind::Image, "image/png", ONE_PIXEL_PNG)
        .expect("persist verified image receipt");

    // A continuation/failover segment has a distinct wire id but still belongs
    // to the original user-visible turn.
    let root_turn_id = MessageId::new().into_string();
    let wire_segment_id = MessageId::new().into_string();
    assert_ne!(root_turn_id, wire_segment_id);
    insert_turn_parent(&repo, &conversation_id, &root_turn_id).await;

    let bus = Arc::new(BroadcastEventBus::new(64));
    let mut live_rx = bus.subscribe_user();
    let (tx, _) = broadcast::channel(64);
    let relay = StreamRelay::new(
        conversation_id.clone(),
        wire_segment_id.clone(),
        owner.clone(),
        repo.clone(),
        bus,
        None,
    )
    .with_root_turn_id(root_turn_id.clone())
    .with_artifact_workspace(workspace.path());
    let rx = tx.subscribe();

    // The provider's active snapshot establishes the artifact contract and
    // carries metadata/content that the prompt-boundary completion omits.
    // Inline receipts stay private in the ACP translator until a terminal
    // completion, so this relay-visible progress frame deliberately has none.
    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "session-imagegen".into(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCall,
            tool_call_id: "imagegen-call".into(),
            status: Some(AcpToolCallStatus::InProgress),
            title: Some("image_gen".into()),
            kind: Some(AcpToolCallKind::Execute),
            raw_input: Some(json!({"prompt": "a child-safe illustrated boy"})),
            raw_output: Some(json!({"phase": "rendered"})),
            content: Some(vec![AcpToolCallContentItem::Content {
                content: AcpToolCallTextBlock {
                    block_type: AcpToolCallTextBlockType::Text,
                    text: "render complete".into(),
                },
            }]),
            locations: Some(vec![]),
        },
        meta: None,
    }))
    .expect("send active ACP tool snapshot");

    // This is the intentionally sparse Completed frame synthesized at the
    // ordered ACP PromptResponse boundary. The relay must materialize it
    // against the active snapshot before its artifact 2PC commit.
    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "session-imagegen".into(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
            tool_call_id: "imagegen-call".into(),
            status: Some(AcpToolCallStatus::Completed),
            title: None,
            kind: None,
            raw_input: None,
            raw_output: None,
            content: Some(vec![AcpToolCallContentItem::Artifact {
                artifact: artifact.clone(),
                source_uri: None,
            }]),
            locations: None,
        },
        meta: None,
    }))
    .expect("send sparse synthesized completion");
    tx.send(AgentStreamEvent::Text(TextEventData {
        content: "已生成。".into(),
    }))
    .expect("send final assistant text");
    tx.send(AgentStreamEvent::Finish(FinishEventData {
        session_id: Some("session-imagegen".into()),
        stop_reason: Some(TurnStopReason::EndTurn),
    }))
        .expect("send enclosing turn finish");

    let outcome = relay.consume(rx).await;
    assert_eq!(outcome.terminal, RelayTerminal::Finish);

    // Query the real SQLite repository, as the history endpoint does before
    // DTO normalization. There must be no provisional row left behind.
    let history = repo
        .get_messages(&conversation_id, 1, 100, SortOrder::Asc)
        .await
        .expect("load durable history");
    let acp_rows = history
        .items
        .iter()
        .filter(|row| row.r#type == "acp_tool_call")
        .collect::<Vec<_>>();
    assert_eq!(acp_rows.len(), 1, "one canonical ACP lifecycle row");
    let row = acp_rows[0];
    assert_eq!(row.msg_id.as_deref(), Some(root_turn_id.as_str()));
    assert_eq!(row.status.as_deref(), Some("finish"));
    let content: Value = serde_json::from_str(&row.content).expect("parse ACP history row");
    assert_eq!(content["turn_id"], root_turn_id);
    assert_eq!(content["artifact_delivery_committed"], true);
    assert_eq!(content["update"]["status"], "completed");
    assert_eq!(content["update"]["title"], "image_gen");
    assert_eq!(content["update"]["kind"], "execute");
    assert_eq!(
        content["update"]["raw_input"]["prompt"],
        "a child-safe illustrated boy"
    );
    assert_eq!(content["update"]["raw_output"]["phase"], "rendered");
    assert!(
        content["update"]["content"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["type"] == "content")),
        "sparse completion must preserve non-artifact tool content"
    );
    let receipts = artifact_items(&content);
    assert_eq!(receipts.len(), 1);
    store
        .reverify_receipt(&receipts[0])
        .expect("hydrated artifact receipt remains locatable and valid");
    assert!(
        history
            .items
            .iter()
            .all(|row| row.status.as_deref() != Some("work")),
        "a successful terminal boundary must not leave in-progress history"
    );
    assert!(
        history.items.iter().all(|row| row.r#type != "tips"),
        "verified delivery plus EndTurn must not synthesize an error card"
    );

    let mut stream_events = Vec::new();
    while let Ok(envelope) = live_rx.try_recv() {
        if envelope.event.name == "message.stream" {
            stream_events.push(envelope.event.data);
        }
    }
    assert!(
        stream_events
            .iter()
            .all(|event| event["turn_id"] == root_turn_id)
    );
    let completed_index = stream_events
        .iter()
        .position(|event| {
            event["type"] == "acp_tool_call"
                && event["data"]["update"]["status"] == "completed"
        })
        .expect("live committed ACP completion");
    let finish_index = stream_events
        .iter()
        .position(|event| event["type"] == "finish")
        .expect("live enclosing finish");
    assert!(completed_index < finish_index, "artifact commit must publish before Finish");
    assert!(stream_events.iter().all(|event| event["type"] != "error"));
}

#[tokio::test]
async fn enclosing_finish_closes_an_unfinished_acp_projection_in_real_history() {
    let (repo, _db, conversation_id, owner) = setup_repo().await;
    let root_turn_id = MessageId::new().into_string();
    insert_turn_parent(&repo, &conversation_id, &root_turn_id).await;
    let wire_segment_id = MessageId::new().into_string();
    let bus = Arc::new(BroadcastEventBus::new(32));
    let (tx, _) = broadcast::channel(32);
    let relay = StreamRelay::new(
        conversation_id.clone(),
        wire_segment_id,
        owner,
        repo.clone(),
        bus,
        None,
    )
    .with_root_turn_id(root_turn_id.clone());
    let rx = tx.subscribe();

    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "session-unfinished".into(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCall,
            tool_call_id: "unfinished-execute".into(),
            status: Some(AcpToolCallStatus::InProgress),
            title: Some("execute".into()),
            kind: Some(AcpToolCallKind::Execute),
            raw_input: Some(json!({"command": "long-running-command"})),
            raw_output: None,
            content: None,
            locations: None,
        },
        meta: None,
    }))
    .expect("send unfinished ACP tool");
    tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
        .expect("send enclosing finish");

    let outcome = relay.consume(rx).await;
    assert_eq!(outcome.terminal, RelayTerminal::Finish);

    let history = repo
        .get_messages(&conversation_id, 1, 100, SortOrder::Asc)
        .await
        .expect("load durable history");
    let row = history
        .items
        .iter()
        .find(|row| row.r#type == "acp_tool_call")
        .expect("persisted ACP lifecycle row");
    assert_eq!(row.msg_id.as_deref(), Some(root_turn_id.as_str()));
    assert_eq!(row.status.as_deref(), Some("error"));
    let content: Value = serde_json::from_str(&row.content).expect("parse ACP history row");
    assert_eq!(content["turn_id"], root_turn_id);
    assert_eq!(content["update"]["status"], "failed");
    assert!(
        history
            .items
            .iter()
            .all(|row| row.status.as_deref() != Some("work")),
        "terminal finalization must not leave an active history projection"
    );
}

#[tokio::test]
async fn continuation_reused_call_ids_keep_distinct_wire_rows_with_one_root_owner() {
    let (repo, _db, conversation_id, owner) = setup_repo().await;
    let root_turn_id = MessageId::new().into_string();
    insert_turn_parent(&repo, &conversation_id, &root_turn_id).await;

    for index in 0..2 {
        let wire_segment_id = MessageId::new().into_string();
        let bus = Arc::new(BroadcastEventBus::new(32));
        let (tx, _) = broadcast::channel(32);
        let relay = StreamRelay::new(
            conversation_id.clone(),
            wire_segment_id,
            owner.clone(),
            repo.clone(),
            bus,
            None,
        )
        .with_root_turn_id(root_turn_id.clone());
        let rx = tx.subscribe();

        // ACP call ids need only be unique within one provider prompt. The
        // second continuation deliberately reuses the same id.
        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: format!("session-continuation-{index}"),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCall,
                tool_call_id: "provider-local-call-1".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: Some("execute".into()),
                kind: Some(AcpToolCallKind::Execute),
                raw_input: Some(json!({"segment": index})),
                raw_output: Some(json!({"ok": true})),
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .expect("send continuation tool completion");
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: format!("continuation {index}"),
        }))
        .expect("send continuation assistant text");
        tx.send(AgentStreamEvent::Finish(FinishEventData::default()))
            .expect("send continuation finish");

        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.terminal, RelayTerminal::Finish);
    }

    let history = repo
        .get_messages(&conversation_id, 1, 100, SortOrder::Asc)
        .await
        .expect("load continuation history");
    let acp_rows = history
        .items
        .iter()
        .filter(|row| row.r#type == "acp_tool_call")
        .collect::<Vec<_>>();
    assert_eq!(
        acp_rows.len(),
        2,
        "wire-scoped correlation must not collapse reused provider call ids"
    );
    assert_ne!(
        acp_rows[0].message_id,
        acp_rows[1].message_id,
        "each persisted wire-scoped lifecycle row has its own business message UUIDv7"
    );
    for row in &acp_rows {
        MessageId::parse(&row.message_id).expect("ACP lifecycle message id is a canonical UUIDv7");
    }
    for row in acp_rows {
        assert_eq!(row.msg_id.as_deref(), Some(root_turn_id.as_str()));
        assert_eq!(row.status.as_deref(), Some("finish"));
        let content: Value = serde_json::from_str(&row.content).expect("parse ACP row");
        assert_eq!(content["turn_id"], root_turn_id);
    }

    let text_rows = history
        .items
        .iter()
        .filter(|row| row.r#type == "text")
        .collect::<Vec<_>>();
    assert_eq!(text_rows.len(), 2);
    for row in text_rows {
        let content: Value = serde_json::from_str(&row.content).expect("parse assistant text row");
        assert_eq!(
            content["turn_id"], root_turn_id,
            "history must retain the logical owner of delayed continuation text"
        );
    }
}
