use nomifun_db::{
    ConversationFilters, ConversationRowUpdate, IConversationRepository, MessageRowUpdate, SortOrder,
    SqliteConversationRepository, TurnArtifactMessageCommit, models::ConversationRow,
    models::MessageRow,
};
use nomifun_common::{ConversationId, MessageId};

const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";

async fn init_database_memory() -> Result<nomifun_db::Database, nomifun_db::DbError> {
    nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(USER_ID.to_owned()).expect("canonical fixture owner"),
    )
    .await
}

async fn setup() -> (SqliteConversationRepository, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (\
            ?, 'openai', 'Fixture provider', 'https://example.invalid', \
            'encrypted', '[]', 1, '[]', 0, 0\
         )",
    )
    .bind(PROVIDER_ID)
    .execute(db.pool())
    .await
    .unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    (repo, db)
}

fn make_conversation(suffix: &str) -> ConversationRow {
    let now = nomifun_common::now_ms();
    ConversationRow {
        id: 0,
        conversation_id: ConversationId::new().into_string(),
        user_id: USER_ID.to_string(),
        name: format!("Conversation {suffix}"),
        r#type: "acp".to_string(),
        extra: r#"{"workspace":"/home/user/project"}"#.to_string(),
        delegation_policy: "automatic".to_string(),
        execution_model_pool: None,
        decision_policy: "automatic".to_string(),
        execution_template_id: None,
        model: Some(format!(
            r#"{{"provider_id":"{PROVIDER_ID}","model":"claude-sonnet-4-20250514"}}"#
        )),
        status: Some("pending".to_string()),
        source: Some("nomifun".to_string()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        created_at: now,
        updated_at: now,
    }
}

fn make_message(conv_id: &str, content: &str) -> MessageRow {
    let now = nomifun_common::now_ms();
    let message_id = MessageId::new().into_string();
    MessageRow {
        id: 0,
        message_id: message_id.clone(),
        conversation_id: conv_id.to_owned(),
        msg_id: Some(message_id),
        r#type: "text".to_string(),
        content: format!(r#"{{"content":"{content}"}}"#),
        position: Some("right".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now,
    }
}

async fn insert_turn_message(
    repo: &SqliteConversationRepository,
    conversation_id: &str,
    message_id: &str,
) {
    repo.insert_message(&MessageRow {
        id: 0,
        message_id: message_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        msg_id: Some(message_id.to_owned()),
        r#type: "text".to_owned(),
        content: r#"{"content":"turn"}"#.to_owned(),
        position: Some("right".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: 1,
    })
    .await
    .unwrap();
}

fn make_generic_artifact_commit(
    message_id: String,
    turn_message_id: &str,
    call_id: &str,
    artifact_id: &str,
) -> TurnArtifactMessageCommit {
    TurnArtifactMessageCommit {
        message_id,
        message_type: "tool_call".to_owned(),
        content: serde_json::json!({
            "call_id": call_id,
            "name": "ImageGeneration",
            "args": {"prompt": "cat"},
            "status": "completed",
            "output": "generated",
            "artifacts": [{
                "id": artifact_id,
                "kind": "image",
                "mime_type": "image/png",
                "path": format!("/workspace/{artifact_id}.png"),
                "relative_path": format!("nomifun-artifacts/{artifact_id}.png"),
                "size_bytes": 10,
                "sha256": "a".repeat(64),
            }],
            "turn_id": turn_message_id,
            "artifact_delivery_committed": true,
        })
        .to_string(),
    }
}

fn make_acp_artifact_commit(
    message_id: String,
    turn_message_id: &str,
    call_id: &str,
    artifact_id: &str,
) -> TurnArtifactMessageCommit {
    TurnArtifactMessageCommit {
        message_id,
        message_type: "acp_tool_call".to_owned(),
        content: serde_json::json!({
            "session_id": "session-1",
            "update": {
                "session_update": "tool_call_update",
                "tool_call_id": call_id,
                "status": "completed",
                "title": "Generate image",
                "content": [{
                    "type": "artifact",
                    "artifact": {
                        "id": artifact_id,
                        "kind": "image",
                        "mime_type": "image/png",
                        "path": format!("/workspace/{artifact_id}.png"),
                        "relative_path": format!("nomifun-artifacts/{artifact_id}.png"),
                        "size_bytes": 10,
                        "sha256": "b".repeat(64),
                    }
                }]
            },
            "turn_id": turn_message_id,
            "artifact_delivery_committed": true,
        })
        .to_string(),
    }
}

fn make_provisional_artifact_message(
    conversation_id: &str,
    turn_message_id: &str,
    commit: &TurnArtifactMessageCommit,
    created_at: i64,
) -> MessageRow {
    let mut content: serde_json::Value = serde_json::from_str(&commit.content).unwrap();
    content["artifact_delivery_committed"] = serde_json::json!(false);
    match commit.message_type.as_str() {
        "tool_call" => {
            content["status"] = serde_json::json!("running");
            content["artifacts"] = serde_json::json!([]);
        }
        "acp_tool_call" => {
            content["update"]["status"] = serde_json::json!("in_progress");
            content["update"]["content"] = serde_json::json!([]);
        }
        _ => unreachable!("test fixture only supports artifact tool messages"),
    }
    MessageRow {
        id: 0,
        message_id: commit.message_id.clone(),
        conversation_id: conversation_id.to_owned(),
        msg_id: Some(turn_message_id.to_owned()),
        r#type: commit.message_type.clone(),
        content: content.to_string(),
        position: Some("left".to_owned()),
        status: Some("work".to_owned()),
        hidden: false,
        created_at,
    }
}

fn make_artifact(conv_id: &str, cron_job_id: &str) -> nomifun_db::ConversationArtifactRow {
    nomifun_db::ConversationArtifactRow {
        conversation_artifact_id: nomifun_common::generate_id(),
        conversation_id: conv_id.to_owned(),
        cron_job_id: Some(cron_job_id.to_owned()),
        kind: "skill_suggest".to_string(),
        status: "pending".to_string(),
        payload: serde_json::json!({
            "cron_job_id": cron_job_id,
            "name": "daily-report",
            "description": "Daily report",
            "skillContent": "---\nname: daily-report\n---\nUse it."
        })
        .to_string(),
        created_at: 1000,
        updated_at: 1000,
    }
}

/// Insert a minimal local cron job and return its logical UUIDv7 ID.
async fn seed_cron_job(pool: &sqlx::SqlitePool) -> String {
    let cron_job_id = nomifun_common::CronJobId::new().into_string();
    sqlx::query_scalar(
        "INSERT INTO cron_jobs \
            (cron_job_id, user_id, name, schedule_kind, schedule_value, payload_message, agent_type, created_by, created_at, updated_at) \
         VALUES (?, ?, 'Job', 'every', '60000', 'msg', 'acp', 'user', 0, 0) \
         RETURNING cron_job_id",
    )
    .bind(&cron_job_id)
    .bind(USER_ID)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── Conversation CRUD ───────────────────────────────────────────────

#[tokio::test]
async fn create_get_update_delete_lifecycle() {
    let (repo, _db) = setup().await;

    // Create
    let mut conv = make_conversation("lifecycle");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    // Get
    let found = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert_eq!(found.name, "Conversation lifecycle");
    assert_eq!(found.status.as_deref(), Some("pending"));

    // Update
    let now = nomifun_common::now_ms();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            name: Some("Updated Name".to_string()),
            status: Some("running".to_string()),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let updated = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert_eq!(updated.name, "Updated Name");
    assert_eq!(updated.status.as_deref(), Some("running"));

    // Delete
    repo.delete(&conv.conversation_id).await.unwrap();
    assert!(repo.get(&conv.conversation_id).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_conversation_cleans_up_messages() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("cascade");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    // Insert messages
    for i in 0..3 {
        let msg = make_message(&conv.conversation_id, &format!("msg {i}"));
        repo.insert_message(&msg).await.unwrap();
    }

    // Verify messages exist
    let msgs = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
    assert_eq!(msgs.total, 3);

    // Repository-owned logical cleanup removes dependent messages.
    repo.delete(&conv.conversation_id).await.unwrap();

    let msgs = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
    assert_eq!(msgs.total, 0);
}

#[tokio::test]
async fn internal_creation_and_delivery_operations_are_durable_and_repository_idempotent() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("durable-operation");

    let (conversation_id, created_now) = repo
        .create_idempotent(&conversation, "attempt:create:1")
        .await
        .unwrap();
    assert!(created_now);
    let (replayed_id, replay_created_now) = repo
        .create_idempotent(&conversation, "attempt:create:1")
        .await
        .unwrap();
    assert_eq!(replayed_id, conversation_id);
    assert!(!replay_created_now);
    assert_eq!(
        repo.find_by_creation_key(USER_ID, "attempt:create:1")
            .await
            .unwrap()
            .unwrap()
            .conversation_id,
        conversation_id
    );
    let accepted_at = nomifun_common::now_ms();
    let request = r#"{"content":"continue"}"#;
    let accepted = repo
        .claim_delivery_receipt(
            USER_ID,
            &conversation_id,
            "decision:1",
            "turn",
            request,
            accepted_at,
        )
        .await
        .unwrap();
    assert_eq!(accepted.status, "accepted");
    assert_eq!(accepted.result_ok, None);
    assert!(nomifun_common::MessageId::parse(&accepted.message_id).is_ok());

    let replayed = repo
        .claim_delivery_receipt(
            USER_ID,
            &conversation_id,
            "decision:1",
            "turn",
            request,
            accepted_at + 1,
        )
        .await
        .unwrap();
    assert_eq!(replayed.status, "accepted");
    assert_eq!(replayed.message_id, accepted.message_id);

    assert!(
        repo.claim_delivery_receipt(
            USER_ID,
            &conversation_id,
            "decision:1",
            "turn",
            r#"{"content":"different"}"#,
            accepted_at + 1,
        )
        .await
        .is_err(),
        "a stable operation cannot be rebound to a different request"
    );

    assert!(
        repo.complete_delivery_receipt(
            USER_ID,
            &conversation_id,
            "decision:1",
            false,
            None,
            Some("terminal provider error"),
            accepted_at + 2,
        )
        .await
        .unwrap()
    );
    assert!(
        repo.complete_delivery_receipt(
            USER_ID,
            &conversation_id,
            "decision:1",
            false,
            None,
            Some("terminal provider error"),
            accepted_at + 3,
        )
        .await
        .unwrap(),
        "settlement replay returns the already committed terminal result"
    );
    let completed = repo
        .claim_delivery_receipt(
            USER_ID,
            &conversation_id,
            "decision:1",
            "turn",
            request,
            accepted_at + 4,
        )
        .await
        .unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.result_ok, Some(false));
    assert_eq!(completed.result_text, None);
    assert_eq!(completed.result_error.as_deref(), Some("terminal provider error"));

    let delete_error = repo.delete(&conversation_id).await.unwrap_err();
    assert!(
        matches!(delete_error, nomifun_db::DbError::Conflict(_)),
        "delivery history keeps the Conversation identity alive"
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_delivery_receipts WHERE operation_id = 'decision:1'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        remaining, 1,
        "Conversation deletion is restricted while a durable receipt exists"
    );
    let remaining_creation_keys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_creation_keys \
         WHERE creation_key = 'attempt:create:1'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        remaining_creation_keys, 1,
        "Conversation deletion is atomic and keeps its creation replay fence"
    );
}

#[tokio::test]
async fn assistant_message_projection_is_atomic_idempotent_and_owner_scoped() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("message-projection");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();

    let now = nomifun_common::now_ms();
    let request = r#"{"execution_id":"exec_1","summary":"done"}"#;
    let mut message = make_message(&conversation.conversation_id, "Execution completed");
    message.position = Some("left".to_owned());
    message.msg_id = Some(message.message_id.clone());
    message.created_at = now;

    let inserted = repo
        .project_assistant_message_with_receipt(
            USER_ID,
            &conversation.conversation_id,
            "execution:exec_1:lead-report",
            "projection",
            request,
            &message,
            now,
        )
        .await
        .unwrap();
    assert!(inserted.inserted);
    assert_eq!(inserted.message.message_id, message.message_id);

    let mut replay_candidate = make_message(&conversation.conversation_id, "must not replace the result");
    replay_candidate.position = Some("left".to_owned());
    replay_candidate.msg_id = Some(replay_candidate.message_id.clone());
    let replayed = repo
        .project_assistant_message_with_receipt(
            USER_ID,
            &conversation.conversation_id,
            "execution:exec_1:lead-report",
            "projection",
            request,
            &replay_candidate,
            now + 1,
        )
        .await
        .unwrap();
    assert!(!replayed.inserted);
    assert_eq!(replayed.message.message_id, message.message_id);
    assert_eq!(replayed.message.content, message.content);

    let message_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(message_count, 1, "a replay must not duplicate the assistant message");

    let mut invalid_shape = replay_candidate.clone();
    invalid_shape.position = Some("right".to_owned());
    let invalid_shape = repo
        .project_assistant_message_with_receipt(
            USER_ID,
            &conversation.conversation_id,
            "execution:exec_invalid:lead-report",
            "projection",
            request,
            &invalid_shape,
            now + 2,
        )
        .await
        .unwrap_err();
    assert!(matches!(invalid_shape, nomifun_db::DbError::Conflict(_)));

    let invalid_kind = repo
        .project_assistant_message_with_receipt(
            USER_ID,
            &conversation.conversation_id,
            "execution:exec_invalid_kind:lead-report",
            "turn",
            request,
            &replay_candidate,
            now + 2,
        )
        .await
        .unwrap_err();
    assert!(matches!(invalid_kind, nomifun_db::DbError::Conflict(_)));

    let payload_conflict = repo
        .project_assistant_message_with_receipt(
            USER_ID,
            &conversation.conversation_id,
            "execution:exec_1:lead-report",
            "projection",
            r#"{"execution_id":"exec_1","summary":"different"}"#,
            &message,
            now + 2,
        )
        .await
        .unwrap_err();
    assert!(matches!(payload_conflict, nomifun_db::DbError::Conflict(_)));

    let non_owner = repo
        .project_assistant_message_with_receipt(
            "0190f5fe-7c00-7a00-8000-000000000099",
            &conversation.conversation_id,
            "execution:exec_2:lead-report",
            "projection",
            request,
            &message,
            now + 3,
        )
        .await
        .unwrap_err();
    assert!(matches!(non_owner, nomifun_db::DbError::NotFound(_)));

    let mut missing_message = message.clone();
    missing_message.conversation_id = "0190f5fe-7c00-7a00-8abc-012345679999".to_owned();
    let missing = repo
        .project_assistant_message_with_receipt(
            USER_ID,
            "0190f5fe-7c00-7a00-8abc-012345679999",
            "execution:exec_3:lead-report",
            "projection",
            request,
            &missing_message,
            now + 4,
        )
        .await
        .unwrap_err();
    assert!(matches!(missing, nomifun_db::DbError::NotFound(_)));
}

// ── Cursor pagination ───────────────────────────────────────────────

#[tokio::test]
async fn cursor_pagination_walks_through_all_items() {
    let (repo, _db) = setup().await;

    // Create 7 conversations with distinct updated_at
    for i in 0..7 {
        let mut c = make_conversation(&format!("{i}"));
        c.updated_at = (i + 1) as i64 * 1000;
        repo.create(&c).await.unwrap();
    }

    // Page 1: no cursor, limit 3
    let p1 = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 3);
    assert!(p1.has_more);
    assert_eq!(p1.total, 7);

    // Page 2
    let cursor = p1.items.last().unwrap().conversation_id.clone();
    let p2 = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cursor: Some(cursor),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p2.items.len(), 3);
    assert!(p2.has_more);

    // Page 3
    let cursor = p2.items.last().unwrap().conversation_id.clone();
    let p3 = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cursor: Some(cursor),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p3.items.len(), 1);
    assert!(!p3.has_more);

    // All 7 items collected, no duplicates
    let mut all_ids: Vec<_> = p1
        .items
        .iter()
        .chain(p2.items.iter())
        .chain(p3.items.iter())
        .map(|c| c.conversation_id.clone())
        .collect();
    all_ids.sort();
    all_ids.dedup();
    assert_eq!(all_ids.len(), 7);
}

// ── Filter combinations ─────────────────────────────────────────────

#[tokio::test]
async fn filter_by_source_and_pinned_combined() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("nomifun-pinned");
    c1.source = Some("nomifun".to_string());
    c1.pinned = true;
    c1.pinned_at = Some(nomifun_common::now_ms());
    c1.conversation_id = repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("telegram-pinned");
    c2.source = Some("telegram".to_string());
    c2.pinned = true;
    c2.pinned_at = Some(nomifun_common::now_ms());
    repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("nomifun-unpinned");
    c3.source = Some("nomifun".to_string());
    c3.pinned = false;
    repo.create(&c3).await.unwrap();

    // Filter: source=nomifun AND pinned=true
    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                source: Some("nomifun".to_string()),
                pinned: Some(true),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].conversation_id, c1.conversation_id);
}

#[tokio::test]
async fn filter_by_cron_job_id() {
    let (repo, db) = setup().await;
    let cron_123 = seed_cron_job(db.pool()).await;
    let cron_456 = seed_cron_job(db.pool()).await;

    let mut c1 = make_conversation("cron-a");
    c1.cron_job_id = Some(cron_123.clone());
    c1.conversation_id = repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("cron-b");
    c2.cron_job_id = Some(cron_456);
    repo.create(&c2).await.unwrap();

    let c3 = make_conversation("no-cron"); // cron_job_id is None
    repo.create(&c3).await.unwrap();

    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cron_job_id: Some(cron_123),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].conversation_id, c1.conversation_id);
}

// ── Extended queries ────────────────────────────────────────────────

#[tokio::test]
async fn find_by_source_and_chat_integration() {
    let (repo, _db) = setup().await;

    let mut c = make_conversation("telegram");
    c.source = Some("telegram".to_string());
    c.channel_chat_id = Some("group:789".to_string());
    c.r#type = "acp".to_string();
    c.conversation_id = repo.create(&c).await.unwrap();

    let found = repo
        .find_by_source_and_chat(USER_ID, "telegram", "group:789", "acp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.conversation_id, c.conversation_id);
}

#[tokio::test]
async fn list_by_cron_job_returns_matching() {
    let (repo, db) = setup().await;
    let job_x = seed_cron_job(db.pool()).await;
    let job_y = seed_cron_job(db.pool()).await;

    let mut c1 = make_conversation("cron1");
    c1.cron_job_id = Some(job_x.clone());
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("cron2");
    c2.cron_job_id = Some(job_x.clone());
    repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("cron3");
    c3.cron_job_id = Some(job_y);
    repo.create(&c3).await.unwrap();

    let result = repo.list_by_cron_job(USER_ID, &job_x).await.unwrap();
    assert_eq!(result.len(), 2);
}

#[tokio::test]
async fn list_associated_finds_same_workspace() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("ws1");
    c1.extra = r#"{"workspace":"/shared"}"#.to_string();
    c1.conversation_id = repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("ws2");
    c2.extra = r#"{"workspace":"/shared"}"#.to_string();
    c2.conversation_id = repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("ws3");
    c3.extra = r#"{"workspace":"/different"}"#.to_string();
    repo.create(&c3).await.unwrap();

    let assoc = repo.list_associated(USER_ID, &c1.conversation_id).await.unwrap();
    assert_eq!(assoc.len(), 1);
    assert_eq!(assoc[0].conversation_id, c2.conversation_id);
}

#[tokio::test]
async fn list_associated_returns_empty_when_no_workspace() {
    let (repo, _db) = setup().await;

    let mut c = make_conversation("no-ws");
    c.extra = r#"{"setting":"value"}"#.to_string();
    c.conversation_id = repo.create(&c).await.unwrap();

    let assoc = repo.list_associated(USER_ID, &c.conversation_id).await.unwrap();
    assert!(assoc.is_empty());
}

// ── Message operations ──────────────────────────────────────────────

#[tokio::test]
async fn message_parent_must_exist_in_the_same_conversation() {
    let (repo, _db) = setup().await;
    let mut first = make_conversation("message-parent-first");
    first.conversation_id = repo.create(&first).await.unwrap();
    let mut second = make_conversation("message-parent-second");
    second.conversation_id = repo.create(&second).await.unwrap();

    let parent = make_message(&first.conversation_id, "parent");
    repo.insert_message(&parent).await.unwrap();

    let mut child = make_message(&first.conversation_id, "child");
    child.msg_id = Some(parent.message_id.clone());
    repo.insert_message(&child).await.unwrap();

    let mut cross_scope = make_message(&second.conversation_id, "cross-scope");
    cross_scope.msg_id = Some(parent.message_id.clone());
    let error = repo.insert_message(&cross_scope).await.unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));

    let mut missing = make_message(&first.conversation_id, "missing");
    missing.msg_id = Some(MessageId::new().into_string());
    let error = repo.insert_message(&missing).await.unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));
}

#[tokio::test]
async fn message_correlation_claim_is_canonical_stable_and_turn_scoped() {
    let (repo, db) = setup().await;
    let mut conv = make_conversation("message-correlation");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let turn_a = MessageId::new().into_string();
    let turn_b = MessageId::new().into_string();

    let first = repo
        .claim_message_correlation(&conv.conversation_id, &turn_a, "tool_call", "provider-call-1")
        .await
        .unwrap();
    let replay = repo
        .claim_message_correlation(&conv.conversation_id, &turn_a, "tool_call", "provider-call-1")
        .await
        .unwrap();
    let other_turn = repo
        .claim_message_correlation(&conv.conversation_id, &turn_b, "tool_call", "provider-call-1")
        .await
        .unwrap();

    MessageId::parse(&first).expect("claimed correlation ID must be canonical");
    assert_eq!(replay, first);
    assert_ne!(other_turn, first);

    let reserved_without_projected_turn: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_correlations \
         WHERE conversation_id = ? AND turn_message_id = ?",
    )
    .bind(&conv.conversation_id)
    .bind(&turn_a)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        reserved_without_projected_turn, 1,
        "correlation reservation must not require a messages row for the wire owner"
    );
}

#[tokio::test]
async fn artifact_message_commit_promotes_inserts_and_replays_atomically() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("artifact-commit");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let turn_id = MessageId::new().into_string();
    insert_turn_message(&repo, &conv.conversation_id, &turn_id).await;
    let generic = make_generic_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "generic-call",
        "generic-image",
    );
    let acp = make_acp_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "acp-call",
        "acp-image",
    );
    let provisional = make_provisional_artifact_message(&conv.conversation_id, &turn_id, &generic, 100);
    repo.insert_message(&provisional).await.unwrap();

    let empty_error = repo
        .commit_turn_artifact_messages(&conv.conversation_id, &turn_id, &[], 200)
        .await
        .unwrap_err();
    assert!(matches!(empty_error, nomifun_db::DbError::Conflict(_)));

    let committed = repo
        .commit_turn_artifact_messages(&conv.conversation_id, &turn_id, &[generic.clone(), acp.clone()], 200)
        .await
        .unwrap();
    assert_eq!(committed.len(), 2);
    assert_eq!(committed[0].message_id, generic.message_id);
    assert_eq!(committed[0].status.as_deref(), Some("finish"));
    assert_eq!(committed[0].content, generic.content);
    assert_eq!(committed[0].created_at, 100, "promotion preserves provisional creation time");
    assert_eq!(committed[1].message_id, acp.message_id);
    assert_eq!(committed[1].status.as_deref(), Some("finish"));
    assert_eq!(committed[1].content, acp.content);
    assert_eq!(committed[1].created_at, 200, "missing row is inserted at commit time");

    let replay = repo
        .commit_turn_artifact_messages(&conv.conversation_id, &turn_id, &[generic.clone(), acp.clone()], 999)
        .await
        .unwrap();
    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0].created_at, 100);
    assert_eq!(replay[1].created_at, 200);
    assert_eq!(replay[0].content, generic.content);
    assert_eq!(replay[1].content, acp.content);
}

#[tokio::test]
async fn artifact_message_commit_rolls_back_the_batch_on_error_state() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("artifact-error-rollback");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let turn_id = MessageId::new().into_string();
    insert_turn_message(&repo, &conv.conversation_id, &turn_id).await;
    let would_insert = make_generic_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "insert-before-conflict",
        "new-image",
    );
    let blocked = make_acp_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "already-failed",
        "failed-image",
    );
    let mut error_row = make_provisional_artifact_message(&conv.conversation_id, &turn_id, &blocked, 100);
    error_row.status = Some("error".to_owned());
    let original_error_content = error_row.content.clone();
    repo.insert_message(&error_row).await.unwrap();

    let error = repo
        .commit_turn_artifact_messages(
            &conv.conversation_id,
            &turn_id,
            &[would_insert.clone(), blocked.clone()],
            200,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));
    assert!(
        repo.get_message(&conv.conversation_id, &would_insert.message_id)
            .await
            .unwrap()
            .is_none(),
        "the earlier insert must roll back when a later row conflicts"
    );
    let still_error = repo.get_message(&conv.conversation_id, &blocked.message_id).await.unwrap().unwrap();
    assert_eq!(still_error.status.as_deref(), Some("error"));
    assert_eq!(still_error.content, original_error_content);
}

#[tokio::test]
async fn artifact_message_commit_rejects_cross_turn_and_wrong_type_rows() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("artifact-identity-conflicts");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let mut other_conv = make_conversation("artifact-other-owner");
    other_conv.conversation_id = repo.create(&other_conv).await.unwrap();
    let turn_id = MessageId::new().into_string();
    let other_turn_id = MessageId::new().into_string();
    let other_conversation_turn_id = MessageId::new().into_string();
    insert_turn_message(&repo, &conv.conversation_id, &turn_id).await;
    insert_turn_message(&repo, &conv.conversation_id, &other_turn_id).await;
    insert_turn_message(
        &repo,
        &other_conv.conversation_id,
        &other_conversation_turn_id,
    )
    .await;

    let cross_conversation = make_acp_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "cross-conversation-call",
        "cross-conversation-image",
    );
    let cross_conversation_row = make_provisional_artifact_message(
        &other_conv.conversation_id,
        &other_conversation_turn_id,
        &make_acp_artifact_commit(
            cross_conversation.message_id.clone(),
            &other_conversation_turn_id,
            "cross-conversation-call",
            "cross-conversation-image",
        ),
        100,
    );
    repo.insert_message(&cross_conversation_row).await.unwrap();
    let error = repo
        .commit_turn_artifact_messages(
            &conv.conversation_id,
            &turn_id,
            std::slice::from_ref(&cross_conversation),
            200,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));

    let cross_turn = make_generic_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "cross-turn-call",
        "cross-turn-image",
    );
    let cross_turn_row = make_provisional_artifact_message(
        &conv.conversation_id,
        &other_turn_id,
        &make_generic_artifact_commit(
            cross_turn.message_id.clone(),
            &other_turn_id,
            "cross-turn-call",
            "cross-turn-image",
        ),
        100,
    );
    repo.insert_message(&cross_turn_row).await.unwrap();
    let error = repo
        .commit_turn_artifact_messages(&conv.conversation_id, &turn_id, std::slice::from_ref(&cross_turn), 200)
        .await
        .unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));

    let wrong_type = make_generic_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "wrong-type-call",
        "wrong-type-image",
    );
    let row = MessageRow {
        id: 0,
        message_id: wrong_type.message_id.clone(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: Some(turn_id.clone()),
        r#type: "text".to_owned(),
        content: serde_json::json!({"content": "not a tool", "turn_id": turn_id}).to_string(),
        position: Some("left".to_owned()),
        status: Some("work".to_owned()),
        hidden: false,
        created_at: 100,
    };
    repo.insert_message(&row).await.unwrap();
    let error = repo
        .commit_turn_artifact_messages(&conv.conversation_id, &turn_id, std::slice::from_ref(&wrong_type), 200)
        .await
        .unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));

    let mut unsupported = make_generic_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "unsupported-type-call",
        "unsupported-type-image",
    );
    unsupported.message_type = "text".to_owned();
    let error = repo
        .commit_turn_artifact_messages(&conv.conversation_id, &turn_id, &[unsupported], 200)
        .await
        .unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));
}

#[tokio::test]
async fn artifact_message_commit_rolls_back_on_different_finished_projection() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("artifact-finish-conflict");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let turn_id = MessageId::new().into_string();
    insert_turn_message(&repo, &conv.conversation_id, &turn_id).await;
    let would_insert = make_acp_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "insert-before-finish-conflict",
        "new-acp-image",
    );
    let existing = make_generic_artifact_commit(
        MessageId::new().into_string(),
        &turn_id,
        "same-call",
        "original-image",
    );
    let different = make_generic_artifact_commit(
        existing.message_id.clone(),
        &turn_id,
        "same-call",
        "different-image",
    );
    let existing_row = MessageRow {
        id: 0,
        message_id: existing.message_id.clone(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: Some(turn_id.clone()),
        r#type: existing.message_type.clone(),
        content: existing.content.clone(),
        position: Some("left".to_owned()),
        status: Some("finish".to_owned()),
        hidden: false,
        created_at: 100,
    };
    repo.insert_message(&existing_row).await.unwrap();

    let error = repo
        .commit_turn_artifact_messages(
            &conv.conversation_id,
            &turn_id,
            &[would_insert.clone(), different],
            200,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, nomifun_db::DbError::Conflict(_)));
    assert!(repo
        .get_message(&conv.conversation_id, &would_insert.message_id)
        .await
        .unwrap()
        .is_none());
    let preserved = repo.get_message(&conv.conversation_id, &existing.message_id).await.unwrap().unwrap();
    assert_eq!(preserved.content, existing.content);
    assert_eq!(preserved.status.as_deref(), Some("finish"));
}

#[tokio::test]
async fn message_pagination_and_ordering() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("msgs");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    for i in 0..10 {
        let mut msg = make_message(&conv.conversation_id, &format!("item {i}"));
        msg.created_at = (i + 1) as i64 * 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    // DESC page 1
    let p1 = repo.get_messages(&conv.conversation_id, 1, 3, SortOrder::Desc).await.unwrap();
    assert_eq!(p1.items.len(), 3);
    assert_eq!(p1.total, 10);
    assert!(p1.has_more);
    assert!(p1.items[0].created_at > p1.items[1].created_at);

    // ASC page 1
    let asc = repo.get_messages(&conv.conversation_id, 1, 3, SortOrder::Asc).await.unwrap();
    assert!(asc.items[0].created_at < asc.items[1].created_at);
}

#[tokio::test]
async fn update_message_fields() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("msg-update");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    let msg = make_message(&conv.conversation_id, "original");
    repo.insert_message(&msg).await.unwrap();

    repo.update_message(
        &msg.message_id,
        &MessageRowUpdate {
            content: Some(r#"{"content":"modified"}"#.to_string()),
            hidden: Some(true),
            status: Some(Some("error".to_string())),
        },
    )
    .await
    .unwrap();

    let msgs = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
    let updated = &msgs.items[0];
    assert_eq!(updated.content, r#"{"content":"modified"}"#);
    assert!(updated.hidden);
    assert_eq!(updated.status.as_deref(), Some("error"));
}

#[tokio::test]
async fn delete_messages_by_conversation_clears_all() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("msg-delete");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    for i in 0..5 {
        let msg = make_message(&conv.conversation_id, &format!("msg {i}"));
        repo.insert_message(&msg).await.unwrap();
    }

    repo.delete_messages_by_conversation(&conv.conversation_id).await.unwrap();

    let result = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Desc).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
}

#[tokio::test]
async fn get_message_by_msg_id_triple() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("msg-find");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    let parent = make_message(&conv.conversation_id, "parent");
    repo.insert_message(&parent).await.unwrap();

    let mut msg = make_message(&conv.conversation_id, "findable");
    msg.msg_id = Some(parent.message_id.clone());
    msg.r#type = "tool_call".to_string();
    repo.insert_message(&msg).await.unwrap();

    // Match
    let found = repo
        .get_message_by_msg_id(&conv.conversation_id, &parent.message_id, "tool_call")
        .await
        .unwrap();
    assert!(found.is_some());

    // The parent message itself uses the same msg_id and remains discoverable
    // under its own type.
    let not_found = repo
        .get_message_by_msg_id(&conv.conversation_id, &parent.message_id, "text")
        .await
        .unwrap();
    assert_eq!(
        not_found.as_ref().map(|row| row.message_id.as_str()),
        Some(parent.message_id.as_str())
    );

    // A type not used by either row is absent.
    let not_found = repo
        .get_message_by_msg_id(&conv.conversation_id, &parent.message_id, "tips")
        .await
        .unwrap();
    assert!(not_found.is_none());

    // Wrong conv → None
    let not_found = repo
        .get_message_by_msg_id(
            "0190f5fe-7c00-7a00-8abc-012345679999",
            &parent.message_id,
            "tool_call",
        )
        .await
        .unwrap();
    assert!(not_found.is_none());
}

// ── Message search ──────────────────────────────────────────────────

#[tokio::test]
async fn search_messages_across_conversations() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("search1");
    c1.conversation_id = repo.create(&c1).await.unwrap();
    let mut c2 = make_conversation("search2");
    c2.conversation_id = repo.create(&c2).await.unwrap();

    let msg1 = make_message(&c1.conversation_id, "Rust 代码审查报告");
    repo.insert_message(&msg1).await.unwrap();

    let msg2 = make_message(&c2.conversation_id, "Python 代码审查总结");
    repo.insert_message(&msg2).await.unwrap();

    let msg3 = make_message(&c1.conversation_id, "unrelated content");
    repo.insert_message(&msg3).await.unwrap();

    let result = repo.search_messages(USER_ID, "审查", 1, 20).await.unwrap();
    assert_eq!(result.total, 2);
    assert_eq!(result.items.len(), 2);

    // Verify conversation names are included
    let names: Vec<_> = result.items.iter().map(|r| &r.conversation_name).collect();
    assert!(names.contains(&&"Conversation search1".to_string()));
    assert!(names.contains(&&"Conversation search2".to_string()));
}

#[tokio::test]
async fn search_messages_empty_result() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("empty-search");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    let msg = make_message(&conv.conversation_id, "hello world");
    repo.insert_message(&msg).await.unwrap();

    let result = repo
        .search_messages(USER_ID, "nonexistent_keyword", 1, 20)
        .await
        .unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
    assert!(!result.has_more);
}

#[tokio::test]
async fn search_messages_pagination() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("search-page");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    for i in 0..5 {
        let mut msg = make_message(&conv.conversation_id, &format!("searchable item {i}"));
        msg.created_at = (i + 1) as i64 * 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    let p1 = repo.search_messages(USER_ID, "searchable", 1, 2).await.unwrap();
    assert_eq!(p1.items.len(), 2);
    assert_eq!(p1.total, 5);
    assert!(p1.has_more);

    let p2 = repo.search_messages(USER_ID, "searchable", 2, 2).await.unwrap();
    assert_eq!(p2.items.len(), 2);
    assert!(p2.has_more);

    let p3 = repo.search_messages(USER_ID, "searchable", 3, 2).await.unwrap();
    assert_eq!(p3.items.len(), 1);
    assert!(!p3.has_more);
}

// ── Pinned update flow ──────────────────────────────────────────────

#[tokio::test]
async fn pin_and_unpin_conversation() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("pin-test");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    // Pin
    let pin_time = nomifun_common::now_ms();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            pinned: Some(true),
            pinned_at: Some(Some(pin_time)),
            updated_at: Some(pin_time),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let pinned = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert!(pinned.pinned);
    assert_eq!(pinned.pinned_at, Some(pin_time));

    // Unpin
    let now = nomifun_common::now_ms();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            pinned: Some(false),
            pinned_at: Some(None),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let unpinned = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert!(!unpinned.pinned);
    assert!(unpinned.pinned_at.is_none());
}

// ── Error cases ─────────────────────────────────────────────────────

#[tokio::test]
async fn update_nonexistent_conversation_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo
        .update(
            "0190f5fe-7c00-7a00-8abc-012345679999",
            &ConversationRowUpdate {
                name: Some("x".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, nomifun_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn delete_nonexistent_conversation_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo.delete("0190f5fe-7c00-7a00-8abc-012345679999").await.unwrap_err();
    assert!(matches!(err, nomifun_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn list_associated_nonexistent_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo
        .list_associated(USER_ID, "0190f5fe-7c00-7a00-8abc-012345679999")
        .await
        .unwrap_err();
    assert!(matches!(err, nomifun_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn update_message_nonexistent_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo
        .update_message(
            &MessageId::new().into_string(),
            &MessageRowUpdate {
                hidden: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, nomifun_db::DbError::NotFound(_)));
}

// ── Extra field update ──────────────────────────────────────────────

#[tokio::test]
async fn update_extra_replaces_json() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("extra-update");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    let now = nomifun_common::now_ms();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            extra: Some(r#"{"workspace":"/new","flag":true}"#.to_string()),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let found = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert_eq!(found.extra, r#"{"workspace":"/new","flag":true}"#);
}

#[tokio::test]
async fn get_messages_excludes_legacy_cron_and_skill_suggest_rows() {
    let (repo, _db) = setup().await;
    let mut conv = make_conversation("message-filter");
    conv.conversation_id = repo.create(&conv).await.unwrap();

    repo.insert_message(&make_message(&conv.conversation_id, "visible")).await.unwrap();

    for ty in ["cron_trigger", "skill_suggest"] {
        repo.insert_message(&MessageRow {
            id: 0,
            message_id: MessageId::new().into_string(),
            conversation_id: conv.conversation_id.clone(),
            msg_id: None,
            r#type: ty.into(),
            content: "{}".into(),
            position: Some("center".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: 2000,
        })
        .await
        .unwrap();
    }

    let rows = repo.get_messages(&conv.conversation_id, 1, 50, SortOrder::Asc).await.unwrap();
    assert_eq!(rows.total, 1);
    assert_eq!(rows.items.len(), 1);
    assert_eq!(rows.items[0].r#type, "text");
}

#[tokio::test]
async fn artifact_upsert_list_and_mark_saved() {
    let (repo, db) = setup().await;
    let mut conv = make_conversation("artifact-row");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;

    let inserted = repo
        .upsert_artifact(&make_artifact(&conv.conversation_id, &cron_job_id))
        .await
        .unwrap();
    assert_eq!(inserted.status, "pending");
    let conversation_artifact_id = inserted.conversation_artifact_id.clone();

    let listed = repo.list_artifacts(&conv.conversation_id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].conversation_artifact_id,
        conversation_artifact_id
    );

    let dismissed = repo
        .update_artifact_status(
            &conv.conversation_id,
            &conversation_artifact_id,
            "dismissed",
            2000,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dismissed.status, "dismissed");
    assert_eq!(dismissed.updated_at, 2000);

    let foreign = repo
        .mark_skill_suggest_artifacts_saved(
            "0190f5fe-7c00-7a00-8000-000000000099",
            &cron_job_id,
            2500,
        )
        .await
        .unwrap();
    assert!(foreign.is_empty());
    let unchanged = repo
        .get_artifact(&conv.conversation_id, &conversation_artifact_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.status, "dismissed");
    assert_eq!(unchanged.updated_at, 2000);

    let saved = repo
        .mark_skill_suggest_artifacts_saved(USER_ID, &cron_job_id, 3000)
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].status, "saved");
    assert_eq!(saved[0].updated_at, 3000);
}

#[tokio::test]
async fn delete_artifacts_by_conversation_removes_rows() {
    let (repo, db) = setup().await;
    let mut conv = make_conversation("artifact-delete");
    conv.conversation_id = repo.create(&conv).await.unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;

    repo.upsert_artifact(&make_artifact(&conv.conversation_id, &cron_job_id))
        .await
        .unwrap();

    repo.delete_artifacts_by_conversation(&conv.conversation_id).await.unwrap();

    let listed = repo.list_artifacts(&conv.conversation_id).await.unwrap();
    assert!(listed.is_empty());
}

#[tokio::test]
async fn artifact_upsert_rejects_cross_owner_and_cross_conversation_cron_links() {
    let (repo, db) = setup().await;
    let other_user = "0190f5fe-7c00-7a00-8000-000000000003";
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'artifact-other', 'hash', 0, 0)",
    )
    .bind(other_user)
    .execute(db.pool())
    .await
    .unwrap();

    let mut foreign_conversation = make_conversation("foreign-artifact-owner");
    foreign_conversation.user_id = other_user.to_owned();
    foreign_conversation.conversation_id = repo.create(&foreign_conversation).await.unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;

    let error = repo
        .upsert_artifact(&make_artifact(
            &foreign_conversation.conversation_id,
            &cron_job_id,
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        nomifun_db::DbError::Conflict(message)
            if message.contains("different owners")
    ));

    let mut conversation = make_conversation("bound-artifact-conversation");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    sqlx::query("UPDATE cron_jobs SET conversation_id = ? WHERE cron_job_id = ?")
        .bind(&foreign_conversation.conversation_id)
        .bind(&cron_job_id)
        .execute(db.pool())
        .await
        .unwrap();

    let error = repo
        .upsert_artifact(&make_artifact(
            &conversation.conversation_id,
            &cron_job_id,
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        nomifun_db::DbError::Conflict(message)
            if message.contains("bound to another Conversation")
    ));
}

// ── User isolation ──────────────────────────────────────────────────

#[tokio::test]
async fn list_paginated_scoped_to_user() {
    let (repo, db) = setup().await;

    // Create a second user
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES ('0190f5fe-7c00-7a00-8000-000000000003', 'other', 'hash', 1000, 1000)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let c1 = make_conversation("user1-conv");
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("user2-conv");
    c2.user_id = "0190f5fe-7c00-7a00-8000-000000000003".to_string();
    c2.r#type = "nomi".to_string();
    c2.delegation_policy = "disabled".to_string();
    c2.model = Some(
        r#"{"provider_id":"0190f5fe-7c00-7a00-8000-000000000002","model":"claude-sonnet-4-20250514"}"#.to_string(),
    );
    repo.create(&c2).await.unwrap();

    // User 1 only sees their own
    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].user_id, USER_ID);
}
