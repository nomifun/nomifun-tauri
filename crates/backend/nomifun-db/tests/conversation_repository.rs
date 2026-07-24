use nomifun_db::{
    ConversationFilters, ConversationRowUpdate, CreateTerminalParams, IConversationRepository,
    IRequirementRepository, ITerminalRepository, MessageRowUpdate,
    RequirementConversationTurnAuthority, SortOrder, SqliteConversationRepository,
    SqliteRequirementRepository, SqliteTerminalRepository, TurnArtifactMessageCommit,
    TurnLifecycleTransition, TurnReceiptCompletion, models::ConversationRow, models::MessageRow,
};
use nomifun_common::{ConversationId, MessageId, RequirementId, TerminalId, UserId};
use sha2::{Digest, Sha256};

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

async fn insert_requirement_owner_fixture(
    pool: &sqlx::SqlitePool,
    display_no: i64,
    tag: &str,
    status: &str,
    owner_conversation_id: Option<&str>,
    owner_terminal_id: Option<&str>,
) -> String {
    let requirement_id = RequirementId::new().into_string();
    sqlx::query(
        "INSERT INTO requirements (\
            requirement_id, display_no, title, tag, status, owner_conversation_id, \
            owner_terminal_id, attempt_count, claim_generation, created_at, updated_at\
         ) VALUES (?, ?, 'owner deletion fixture', ?, ?, ?, ?, 0, 0, 10, 10)",
    )
    .bind(&requirement_id)
    .bind(display_no)
    .bind(tag)
    .bind(status)
    .bind(owner_conversation_id)
    .bind(owner_terminal_id)
    .execute(pool)
    .await
    .unwrap();
    requirement_id
}

type RequirementOwnerAuditRow = (
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

async fn requirement_owner_audit_row(
    pool: &sqlx::SqlitePool,
    requirement_id: &str,
) -> RequirementOwnerAuditRow {
    sqlx::query_as(
        "SELECT status, owner_conversation_id, owner_terminal_id, claim_generation, \
                claim_token, active_turn_started_at, lease_expires_at \
         FROM requirements WHERE requirement_id = ?",
    )
    .bind(requirement_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn expose_pre_008_running_exit_fixture(db: &nomifun_db::Database) {
    // Test-only escape hatch for rows that could have been persisted before
    // migration 008 installed the physical Running exit invariant. Production
    // databases retain the trigger; the dedicated authority test below proves
    // that current raw SQL cannot create this state.
    sqlx::query("DROP TRIGGER trg_conversations_running_exit_guard")
        .execute(db.pool())
        .await
        .unwrap();
}

async fn expose_pre_008_running_admission_fixture(db: &nomifun_db::Database) {
    // This isolated in-memory database intentionally models an ambiguous
    // legacy Running row with no exact owner. Never weaken the production
    // migration to make historical recovery tests constructible.
    sqlx::query("DROP TRIGGER trg_conversations_running_admission_guard")
        .execute(db.pool())
        .await
        .unwrap();
}

async fn expose_missing_receipt_corruption_fixture(db: &nomifun_db::Database) {
    // Production receipts are permanent replay evidence. Tests which exercise
    // quarantine behavior for an already-corrupt/missing receipt must
    // explicitly remove that physical guard in their isolated database.
    sqlx::query("DROP TRIGGER trg_conversation_delivery_receipts_no_delete")
        .execute(db.pool())
        .await
        .unwrap();
}

async fn expose_pre_012_receipt_lifecycle_corruption_fixture(db: &nomifun_db::Database) {
    // Production receipts are protected by migration 012. This isolated
    // in-memory escape hatch models a malformed terminal row persisted before
    // that physical state machine existed so recovery can still prove it
    // fails closed.
    sqlx::query("DROP TRIGGER trg_conversation_delivery_receipts_lifecycle_update_guard")
        .execute(db.pool())
        .await
        .unwrap();
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
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let updated = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    assert_eq!(updated.name, "Updated Name");
    assert_eq!(updated.status.as_deref(), Some("pending"));

    // Delete
    repo.delete(&conv.conversation_id).await.unwrap();
    assert!(repo.get(&conv.conversation_id).await.unwrap().is_none());
}

#[tokio::test]
async fn generic_update_allows_metadata_but_rejects_every_lifecycle_status_write() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("generic-lifecycle-rejected");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();

    repo.update(
        &conversation.conversation_id,
        &ConversationRowUpdate {
            name: Some("metadata remains writable".to_owned()),
            pinned: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let metadata = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata.name, "metadata remains writable");
    assert!(metadata.pinned);
    assert_eq!(metadata.status.as_deref(), Some("pending"));

    for forbidden_status in ["running", "finished", "pending"] {
        let error = repo
            .update(
                &conversation.conversation_id,
                &ConversationRowUpdate {
                    name: Some(format!("must roll back with {forbidden_status}")),
                    status: Some(forbidden_status.to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                nomifun_db::DbError::Conflict(ref message)
                    if message.contains("cannot change lifecycle status")
            ),
            "generic status={forbidden_status} must fail closed: {error}"
        );
    }
    let unchanged = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.name, "metadata remains writable");
    assert_eq!(unchanged.status.as_deref(), Some("pending"));
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
async fn session_deletion_retains_only_ambiguous_typed_owners_and_survives_reopen() {
    let database_root = tempfile::tempdir().unwrap();
    let database_path = database_root.path().join("deleted-owner-history.sqlite3");
    let database = nomifun_db::init_database(&database_path).await.unwrap();
    let installation_owner = nomifun_db::installation_owner_id(database.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?, 'openai', 'Fixture provider', 'https://example.invalid', \
                   'encrypted', '[]', 1, '[]', 0, 0)",
    )
    .bind(PROVIDER_ID)
    .execute(database.pool())
    .await
    .unwrap();

    let conversation_repo = SqliteConversationRepository::new(database.pool().clone());
    let requirement_repo = SqliteRequirementRepository::new(database.pool().clone());
    let terminal_repo = SqliteTerminalRepository::new(database.pool().clone());

    let mut conversation = make_conversation("deleted-owner-history");
    conversation.user_id = installation_owner.clone();
    conversation.conversation_id = conversation_repo.create(&conversation).await.unwrap();
    let terminal = terminal_repo
        .create(&CreateTerminalParams {
            id: TerminalId::new(),
            name: "deleted owner history".to_owned(),
            cwd: ".".to_owned(),
            command: "shell".to_owned(),
            args: "[]".to_owned(),
            env: None,
            backend: None,
            mode: None,
            cols: 80,
            rows: 24,
            user_id: UserId::parse(installation_owner.clone()).unwrap(),
        })
        .await
        .unwrap();

    let conversation_inactive_id = insert_requirement_owner_fixture(
        database.pool(),
        910_001,
        "deleted-conversation-inactive",
        "failed",
        Some(&conversation.conversation_id),
        None,
    )
    .await;
    let conversation_review_id = insert_requirement_owner_fixture(
        database.pool(),
        910_002,
        "deleted-conversation-review",
        "pending",
        None,
        None,
    )
    .await;
    let conversation_review_claim = requirement_repo
        .claim_next_for_runner(
            "deleted-conversation-review",
            Some(&conversation.conversation_id),
            None,
            10_000,
            100,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        conversation_review_claim.row.requirement_id,
        conversation_review_id
    );
    sqlx::query(
        "UPDATE requirements \
         SET status = 'needs_review', lease_expires_at = NULL, \
             completion_note = 'parked before Conversation deletion' \
         WHERE requirement_id = ?",
    )
    .bind(&conversation_review_id)
    .execute(database.pool())
    .await
    .unwrap();
    let conversation_active_id = insert_requirement_owner_fixture(
        database.pool(),
        910_003,
        "deleted-conversation-active",
        "pending",
        None,
        None,
    )
    .await;
    let conversation_active_claim = requirement_repo
        .claim_next_for_runner(
            "deleted-conversation-active",
            Some(&conversation.conversation_id),
            None,
            10_000,
            200,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        conversation_active_claim.row.requirement_id,
        conversation_active_id
    );

    let terminal_inactive_id = insert_requirement_owner_fixture(
        database.pool(),
        910_004,
        "deleted-terminal-inactive",
        "failed",
        None,
        Some(terminal.terminal_id.as_str()),
    )
    .await;
    let terminal_review_id = insert_requirement_owner_fixture(
        database.pool(),
        910_005,
        "deleted-terminal-review",
        "pending",
        None,
        None,
    )
    .await;
    let terminal_review_claim = requirement_repo
        .claim_next_for_runner(
            "deleted-terminal-review",
            None,
            Some(terminal.terminal_id.as_str()),
            10_000,
            300,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal_review_claim.row.requirement_id, terminal_review_id);
    sqlx::query(
        "UPDATE requirements \
         SET status = 'needs_review', lease_expires_at = NULL, \
             completion_note = 'parked before Terminal deletion' \
         WHERE requirement_id = ?",
    )
    .bind(&terminal_review_id)
    .execute(database.pool())
    .await
    .unwrap();
    let terminal_active_id = insert_requirement_owner_fixture(
        database.pool(),
        910_006,
        "deleted-terminal-active",
        "pending",
        None,
        None,
    )
    .await;
    let terminal_active_claim = requirement_repo
        .claim_next_for_runner(
            "deleted-terminal-active",
            None,
            Some(terminal.terminal_id.as_str()),
            10_000,
            400,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal_active_claim.row.requirement_id, terminal_active_id);

    conversation_repo
        .delete(&conversation.conversation_id)
        .await
        .unwrap();
    terminal_repo
        .delete(terminal.terminal_id.as_str())
        .await
        .unwrap();

    drop((conversation_repo, requirement_repo, terminal_repo));
    database.close().await;
    let reopened = nomifun_db::init_database(&database_path)
        .await
        .expect(
            "conditional historical owner references must pass startup orphan audit after deletion",
        );
    nomifun_db::validate_id_schema_contract(reopened.pool())
        .await
        .unwrap();

    let conversation_parent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE conversation_id = ?")
            .bind(&conversation.conversation_id)
            .fetch_one(reopened.pool())
            .await
            .unwrap();
    let terminal_parent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM terminal_sessions WHERE terminal_id = ?")
            .bind(terminal.terminal_id.as_str())
            .fetch_one(reopened.pool())
            .await
            .unwrap();
    assert_eq!(conversation_parent_count, 0);
    assert_eq!(terminal_parent_count, 0);

    let conversation_inactive =
        requirement_owner_audit_row(reopened.pool(), &conversation_inactive_id).await;
    assert_eq!(conversation_inactive.0, "failed");
    assert_eq!(conversation_inactive.1, None);
    assert_eq!(conversation_inactive.2, None);

    let conversation_review =
        requirement_owner_audit_row(reopened.pool(), &conversation_review_id).await;
    assert_eq!(conversation_review.0, "needs_review");
    assert_eq!(
        conversation_review.1.as_deref(),
        Some(conversation.conversation_id.as_str())
    );
    assert_eq!(conversation_review.2, None);
    assert_eq!(
        conversation_review.3,
        conversation_review_claim.row.claim_generation
    );
    assert_eq!(
        conversation_review.4,
        conversation_review_claim.row.claim_token
    );
    assert_eq!(
        conversation_review.5,
        conversation_review_claim.row.active_turn_started_at
    );
    assert_eq!(conversation_review.6, None);

    let conversation_active =
        requirement_owner_audit_row(reopened.pool(), &conversation_active_id).await;
    assert_eq!(conversation_active.0, "needs_review");
    assert_eq!(
        conversation_active.1.as_deref(),
        Some(conversation.conversation_id.as_str())
    );
    assert_eq!(conversation_active.2, None);
    assert_eq!(
        conversation_active.3,
        conversation_active_claim.row.claim_generation
    );
    assert_eq!(
        conversation_active.4,
        conversation_active_claim.row.claim_token
    );
    assert_eq!(
        conversation_active.5,
        conversation_active_claim.row.active_turn_started_at
    );
    assert_eq!(conversation_active.6, None);

    let terminal_inactive =
        requirement_owner_audit_row(reopened.pool(), &terminal_inactive_id).await;
    assert_eq!(terminal_inactive.0, "failed");
    assert_eq!(terminal_inactive.1, None);
    assert_eq!(terminal_inactive.2, None);

    let terminal_review =
        requirement_owner_audit_row(reopened.pool(), &terminal_review_id).await;
    assert_eq!(terminal_review.0, "needs_review");
    assert_eq!(terminal_review.1, None);
    assert_eq!(
        terminal_review.2.as_deref(),
        Some(terminal.terminal_id.as_str())
    );
    assert_eq!(
        terminal_review.3,
        terminal_review_claim.row.claim_generation
    );
    assert_eq!(terminal_review.4, terminal_review_claim.row.claim_token);
    assert_eq!(
        terminal_review.5,
        terminal_review_claim.row.active_turn_started_at
    );
    assert_eq!(terminal_review.6, None);

    let terminal_active =
        requirement_owner_audit_row(reopened.pool(), &terminal_active_id).await;
    assert_eq!(terminal_active.0, "needs_review");
    assert_eq!(terminal_active.1, None);
    assert_eq!(
        terminal_active.2.as_deref(),
        Some(terminal.terminal_id.as_str())
    );
    assert_eq!(
        terminal_active.3,
        terminal_active_claim.row.claim_generation
    );
    assert_eq!(terminal_active.4, terminal_active_claim.row.claim_token);
    assert_eq!(
        terminal_active.5,
        terminal_active_claim.row.active_turn_started_at
    );
    assert_eq!(terminal_active.6, None);
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
async fn delivery_receipt_claim_has_exactly_one_execution_leader() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("atomic-delivery-claim");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let other_receiver = repo.clone();
    let operation_id = "atomic-delivery-claim:1";
    let request_payload = r#"{"content":"execute once"}"#;
    let claimed_at = nomifun_common::now_ms();

    let (left, right) = tokio::join!(
        repo.claim_delivery_receipt_once(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            "turn",
            request_payload,
            claimed_at,
        ),
        other_receiver.claim_delivery_receipt_once(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            "turn",
            request_payload,
            claimed_at,
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();

    assert_eq!(
        usize::from(left.claimed_new) + usize::from(right.claimed_new),
        1,
        "only the transaction that inserted the receipt may execute"
    );
    assert_eq!(left.receipt.message_id, right.receipt.message_id);
    assert_eq!(left.receipt.status, "accepted");
    assert_eq!(right.receipt.status, "accepted");

    let restart_replay = repo
        .claim_delivery_receipt_once(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            "turn",
            request_payload,
            claimed_at + 1,
        )
        .await
        .unwrap();
    assert!(
        !restart_replay.claimed_new,
        "an accepted receipt is absorbing after receiver restart"
    );
    assert_eq!(
        restart_replay.receipt.message_id,
        left.receipt.message_id
    );
}

#[tokio::test]
async fn atomic_turn_claim_has_one_leader_and_existing_receipt_never_readmits_lifecycle() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("atomic-turn-admission");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let other = repo.clone();
    let operation_id = "atomic-turn-admission:1";
    let request_payload = r#"{"content":"execute exactly once"}"#;
    let now = nomifun_common::now_ms();
    let (left, right) = tokio::join!(
        repo.claim_turn_delivery_receipt_and_admit(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            request_payload,
            0,
            now,
        ),
        other.claim_turn_delivery_receipt_and_admit(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            request_payload,
            0,
            now,
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(
        usize::from(left.claimed_new) + usize::from(right.claimed_new),
        1
    );
    assert_eq!(left.receipt.message_id, right.receipt.message_id);
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "receipt INSERT leadership and Running admission commit together"
    );

    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: request_payload.to_owned(),
                result_ok: true,
                result_text: Some("finished before replay".to_owned()),
                result_error: None,
            },
            now + 1,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    let replay = repo
        .claim_turn_delivery_receipt_and_admit(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            request_payload,
            0,
            now + 2,
        )
        .await
        .unwrap();
    assert!(!replay.claimed_new);
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished"),
        "an existing accepted receipt is absorbing and cannot re-admit Running"
    );
}

#[tokio::test]
async fn public_turn_claim_and_edit_reservation_have_one_sqlite_winner() {
    let database_root = tempfile::tempdir().unwrap();
    let database_path = database_root.path().join("public-vs-edit-race.sqlite3");
    let database_a = nomifun_db::init_database(&database_path).await.unwrap();
    let owner_user_id = nomifun_db::installation_owner_id(database_a.pool())
        .await
        .unwrap();
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
    .execute(database_a.pool())
    .await
    .unwrap();
    let database_b = nomifun_db::init_database(&database_path).await.unwrap();
    // These repositories own distinct SqlitePools over the same durable file,
    // matching two backend processes rather than two Arc/repository clones.
    let normal = SqliteConversationRepository::new(database_a.pool().clone());
    let edit = SqliteConversationRepository::new(database_b.pool().clone());

    let mut conversation = make_conversation("public-vs-edit-race");
    conversation.user_id = owner_user_id.clone();
    conversation.status = Some("finished".to_owned());
    conversation.conversation_id = normal.create(&conversation).await.unwrap();
    let target = make_message(&conversation.conversation_id, "original");
    normal.insert_message(&target).await.unwrap();
    let snapshot = normal
        .get_turn_admission_state(&owner_user_id, &conversation.conversation_id)
        .await
        .unwrap();
    let normal_operation = "public-race:normal";
    let edit_operation = "public-edit-resubmit:v1:race";
    let normal_payload = r#"{"content":"normal"}"#;
    let edit_payload = format!(
        r#"{{"workflow":"edit-resubmit","target_message_id":"{}","content":"edited"}}"#,
        target.message_id
    );
    let edit_candidate_message_id = MessageId::new().into_string();
    let now = nomifun_common::now_ms();
    let (normal_result, edit_result) = tokio::join!(
        normal.claim_turn_delivery_receipt_and_admit(
            &owner_user_id,
            &conversation.conversation_id,
            normal_operation,
            normal_payload,
            snapshot.epoch,
            now,
        ),
        edit.claim_edit_resubmit_receipt_and_fence(
            &owner_user_id,
            &conversation.conversation_id,
            edit_operation,
            &edit_candidate_message_id,
            &edit_payload,
            &target.message_id,
            snapshot.epoch,
            now,
        ),
    );
    assert_eq!(
        usize::from(normal_result.is_ok()) + usize::from(edit_result.is_ok()),
        1,
        "one SQLite transaction must win both receipt leadership and lifecycle/fence authority"
    );
    let receipt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_delivery_receipts \
         WHERE conversation_id = ? AND operation_id IN (?, ?)",
    )
    .bind(&conversation.conversation_id)
    .bind(normal_operation)
    .bind(edit_operation)
    .fetch_one(database_a.pool())
    .await
    .unwrap();
    assert_eq!(receipt_count, 1, "the losing transaction rolls its INSERT back");
}

#[tokio::test]
async fn reset_after_edit_reservation_invalidates_the_old_admit_epoch() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("edit-reserve-reset-race");
    conversation.status = Some("finished".to_owned());
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let target = make_message(&conversation.conversation_id, "original");
    repo.insert_message(&target).await.unwrap();
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let operation_id = "public-edit-resubmit:v1:reset-race";
    let payload = format!(
        r#"{{"workflow":"edit-resubmit","target_message_id":"{}","content":"edited"}}"#,
        target.message_id
    );
    let claim = repo
        .claim_edit_resubmit_receipt_and_fence(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &MessageId::new().into_string(),
            &payload,
            &target.message_id,
            initial.epoch,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    let reserved_epoch = initial.epoch + 1;
    assert_eq!(
        repo.reset_terminal_conversation(
            USER_ID,
            &conversation.conversation_id,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    assert!(
        !repo
            .admit_reserved_edit_turn(
                USER_ID,
                &conversation.conversation_id,
                operation_id,
                &payload,
                reserved_epoch,
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        "reset advances the persistent epoch and removes the reservation marker"
    );
    let state = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert!(state.epoch > reserved_epoch);
    assert_eq!(state.active_operation_id, None);
}

#[tokio::test]
async fn late_a_finalizer_cannot_touch_b_after_external_a_result() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("external-a-then-b");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let now = nomifun_common::now_ms();
    let a_operation = "turn:generation-a";
    let a_payload = r#"{"content":"A"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        a_operation,
        a_payload,
        0,
        now,
    )
    .await
    .unwrap();
    repo.finalize_orphaned_turn(
        USER_ID,
        &conversation.conversation_id,
        "external stop won A",
        now + 1,
    )
    .await
    .unwrap();
    let after_a = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let b_operation = "turn:generation-b";
    let b_payload = r#"{"content":"B"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        b_operation,
        b_payload,
        after_a.epoch,
        now + 2,
    )
    .await
    .unwrap();
    let late_a = repo
        .finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: a_operation.to_owned(),
                kind: "turn".to_owned(),
                request_payload: a_payload.to_owned(),
                result_ok: true,
                result_text: Some("late A".to_owned()),
                result_error: None,
            },
            now + 3,
        )
        .await
        .unwrap();
    assert_eq!(late_a, TurnLifecycleTransition::Stale);
    let state = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(state.active_operation_id.as_deref(), Some(b_operation));
    let b_receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, b_operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b_receipt.status, "accepted");
    let a_receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, a_operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a_receipt.result_ok, Some(false));
}

#[tokio::test]
async fn exact_finalize_adopts_receipt_only_completion_for_its_active_generation() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("receipt-only-active-adopt");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let now = nomifun_common::now_ms();
    let operation_id = "turn:receipt-only-active";
    let payload = r#"{"content":"A"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        payload,
        0,
        now,
    )
    .await
    .unwrap();
    repo.complete_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        false,
        None,
        Some("stop result"),
        now + 1,
    )
    .await
    .unwrap();
    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: payload.to_owned(),
                result_ok: true,
                result_text: Some("late success".to_owned()),
                result_error: None,
            },
            now + 2,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(receipt.result_error.as_deref(), Some("stop result"));
}

#[tokio::test]
async fn exact_finalize_repairs_finished_row_that_still_owns_the_operation() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("finished-active-exact-finalize");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let operation_id = "turn:finished-active-exact";
    let payload = r#"{"content":"work"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        payload,
        0,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();
    let active = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    expose_pre_008_running_exit_fixture(&db).await;
    sqlx::query(
        "UPDATE conversations SET status = 'finished' \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: payload.to_owned(),
                result_ok: true,
                result_text: Some("done".to_owned()),
                result_error: None,
            },
            nomifun_common::now_ms() + 1,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    let repaired = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(repaired.epoch, active.epoch + 1);
    assert_eq!(repaired.active_operation_id, None);
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(true));
}

#[tokio::test]
async fn missing_a_receipt_cannot_release_or_mutate_active_b_generation() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("missing-a-active-b");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let now = nomifun_common::now_ms();
    let b_operation = "turn:missing-a-active-b";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        b_operation,
        r#"{"content":"B"}"#,
        0,
        now,
    )
    .await
    .unwrap();
    let missing = repo
        .finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: "turn:missing-a".to_owned(),
                kind: "turn".to_owned(),
                request_payload: r#"{"content":"A"}"#.to_owned(),
                result_ok: false,
                result_text: None,
                result_error: Some("ambiguous".to_owned()),
            },
            now + 1,
        )
        .await;
    assert!(matches!(missing, Err(nomifun_db::DbError::NotFound(_))));
    let state = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(state.active_operation_id.as_deref(), Some(b_operation));
}

#[tokio::test]
async fn exact_stop_for_a_cannot_finalize_active_b_from_another_service() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("stop-a-vs-b");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let now = nomifun_common::now_ms();
    let a_operation = "turn:stop-a";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        a_operation,
        r#"{"content":"A"}"#,
        0,
        now,
    )
    .await
    .unwrap();
    let captured_a = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    repo.finalize_orphaned_turn(
        USER_ID,
        &conversation.conversation_id,
        "external owner closed A",
        now + 1,
    )
    .await
    .unwrap();
    let after_a = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let b_operation = "turn:stop-b";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        b_operation,
        r#"{"content":"B"}"#,
        after_a.epoch,
        now + 2,
    )
    .await
    .unwrap();
    assert_eq!(
        repo.finalize_exact_cancelled_turn_generation(
            USER_ID,
            &conversation.conversation_id,
            captured_a.epoch,
            captured_a.active_operation_id.as_deref(),
            "late stop A",
            now + 3,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );
    let state = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(state.active_operation_id.as_deref(), Some(b_operation));
    let b_receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, b_operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b_receipt.status, "accepted");
}

#[tokio::test]
async fn exact_stop_repairs_finished_edit_reservations_and_partial_admissions() {
    let (repo, db) = setup().await;
    expose_pre_008_running_exit_fixture(&db).await;

    for partial_admission in [false, true] {
        let mut conversation = make_conversation(if partial_admission {
            "finished-edit-partial-admission"
        } else {
            "finished-edit-reservation"
        });
        conversation.status = Some("finished".to_owned());
        conversation.conversation_id = repo.create(&conversation).await.unwrap();
        let target = make_message(&conversation.conversation_id, "original");
        repo.insert_message(&target).await.unwrap();

        let initial = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        let operation_id = format!(
            "public-edit-resubmit:v1:finished-repair:{}",
            if partial_admission {
                "admitted"
            } else {
                "reserved"
            }
        );
        let payload = format!(
            r#"{{"workflow":"edit-resubmit","target_message_id":"{}","content":"edited"}}"#,
            target.message_id
        );
        let candidate_message_id = MessageId::new().into_string();
        repo.claim_edit_resubmit_receipt_and_fence(
            USER_ID,
            &conversation.conversation_id,
            &operation_id,
            &candidate_message_id,
            &payload,
            &target.message_id,
            initial.epoch,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
        let reserved = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();

        let expected_epoch = if partial_admission {
            assert!(
                repo.admit_reserved_edit_turn(
                    USER_ID,
                    &conversation.conversation_id,
                    &operation_id,
                    &payload,
                    reserved.epoch,
                    nomifun_common::now_ms() + 1,
                )
                .await
                .unwrap()
            );
            let admitted = repo
                .get_turn_admission_state(USER_ID, &conversation.conversation_id)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE conversations SET status = 'finished' \
                 WHERE conversation_id = ? AND user_id = ?",
            )
            .bind(&conversation.conversation_id)
            .bind(USER_ID)
            .execute(db.pool())
            .await
            .unwrap();
            admitted.epoch
        } else {
            reserved.epoch
        };

        assert_eq!(
            repo.finalize_exact_cancelled_turn_generation(
                USER_ID,
                &conversation.conversation_id,
                expected_epoch,
                Some(&operation_id),
                "interrupted edit/resubmit",
                nomifun_common::now_ms() + 2,
            )
            .await
            .unwrap(),
            TurnLifecycleTransition::Committed
        );
        let state = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        assert_eq!(state.epoch, expected_epoch + 1);
        assert_eq!(state.active_operation_id, None);
        let row = repo
            .get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status.as_deref(), Some("finished"));
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
        assert!(extra.get("_edit_resubmit_fence").is_none());
        let receipt = repo
            .get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.status, "completed");
        assert_eq!(receipt.result_ok, Some(false));
    }
}

#[tokio::test]
async fn pre_admission_edit_recovery_loses_cleanly_to_concurrent_admission() {
    let (repo, _db) = setup().await;

    for admission_wins in [false, true] {
        let mut conversation = make_conversation(if admission_wins {
            "edit-recovery-loses"
        } else {
            "edit-recovery-wins"
        });
        conversation.status = Some("finished".to_owned());
        conversation.conversation_id = repo.create(&conversation).await.unwrap();
        let target = make_message(&conversation.conversation_id, "original");
        repo.insert_message(&target).await.unwrap();
        let initial = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        let operation_id = format!(
            "public-edit-resubmit:v1:recovery-race:{}",
            if admission_wins { "admit" } else { "recover" }
        );
        let payload = format!(
            r#"{{"workflow":"edit-resubmit","target_message_id":"{}","content":"edited"}}"#,
            target.message_id
        );
        let candidate_message_id = MessageId::new().into_string();
        repo.claim_edit_resubmit_receipt_and_fence(
            USER_ID,
            &conversation.conversation_id,
            &operation_id,
            &candidate_message_id,
            &payload,
            &target.message_id,
            initial.epoch,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
        let reserved = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        let wrong_candidate = MessageId::new().into_string();
        assert_eq!(
            repo.recover_unadmitted_edit_resubmit_reservation(
                USER_ID,
                &conversation.conversation_id,
                &operation_id,
                &wrong_candidate,
                &payload,
                reserved.epoch,
                "losing edit candidate was cancelled",
                nomifun_common::now_ms() + 1,
            )
            .await
            .unwrap(),
            TurnLifecycleTransition::Stale,
            "a losing caller-minted candidate must not settle the reservation winner"
        );
        assert_eq!(
            repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "accepted"
        );
        if admission_wins {
            assert!(
                repo.admit_reserved_edit_turn(
                    USER_ID,
                    &conversation.conversation_id,
                    &operation_id,
                    &payload,
                    reserved.epoch,
                    nomifun_common::now_ms() + 1,
                )
                .await
                .unwrap()
            );
        }

        let recovery = repo
            .recover_unadmitted_edit_resubmit_reservation(
                USER_ID,
                &conversation.conversation_id,
                &operation_id,
                &candidate_message_id,
                &payload,
                reserved.epoch,
                "process exited before edit admission",
                nomifun_common::now_ms() + 2,
            )
            .await
            .unwrap();
        let receipt = repo
            .get_delivery_receipt(USER_ID, &conversation.conversation_id, &operation_id)
            .await
            .unwrap()
            .unwrap();
        let state = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        if admission_wins {
            assert_eq!(recovery, TurnLifecycleTransition::Stale);
            assert_eq!(receipt.status, "accepted");
            assert_eq!(state.active_operation_id.as_deref(), Some(operation_id.as_str()));
        } else {
            assert_eq!(recovery, TurnLifecycleTransition::Committed);
            assert_eq!(receipt.status, "completed");
            assert_eq!(receipt.result_ok, Some(false));
            assert_eq!(state.active_operation_id, None);
            let row = repo
                .get(&conversation.conversation_id)
                .await
                .unwrap()
                .unwrap();
            let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
            assert!(extra.get("_edit_resubmit_fence").is_none());
        }
    }
}

#[tokio::test]
async fn missing_exact_receipt_quarantines_running_and_finished_active_generations() {
    let (repo, db) = setup().await;
    expose_pre_008_running_exit_fixture(&db).await;
    expose_missing_receipt_corruption_fixture(&db).await;

    for terminal_status in [None, Some("finished")] {
        let mut conversation = make_conversation(terminal_status.unwrap_or("running"));
        conversation.conversation_id = repo.create(&conversation).await.unwrap();
        let operation_id = format!(
            "turn:missing-proof:{}",
            terminal_status.unwrap_or("running")
        );
        repo.claim_turn_delivery_receipt_and_admit(
            USER_ID,
            &conversation.conversation_id,
            &operation_id,
            r#"{"content":"work"}"#,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
        let state = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        if let Some(status) = terminal_status {
            sqlx::query(
                "UPDATE conversations SET status = ? \
                 WHERE conversation_id = ? AND user_id = ?",
            )
            .bind(status)
            .bind(&conversation.conversation_id)
            .bind(USER_ID)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query("DELETE FROM conversation_delivery_receipts WHERE operation_id = ?")
            .bind(&operation_id)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(matches!(
            repo.finalize_orphaned_turn(
                USER_ID,
                &conversation.conversation_id,
                "cannot prove old work stopped",
                nomifun_common::now_ms() + 1,
            )
            .await,
            Err(nomifun_db::DbError::Conflict(_))
        ));
        assert!(matches!(
            repo.finalize_exact_cancelled_turn_generation(
                USER_ID,
                &conversation.conversation_id,
                state.epoch,
                Some(&operation_id),
                "cannot prove old work stopped",
                nomifun_common::now_ms() + 2,
            )
            .await,
            Err(nomifun_db::DbError::Conflict(_))
        ));
        let after = repo
            .get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap();
        assert_eq!(after, state, "quarantine must not mutate lifecycle authority");
    }
}

#[tokio::test]
async fn orphan_recovery_cleans_finished_admitted_edit_receipt_and_fence() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("finished-admitted-edit-orphan");
    conversation.status = Some("finished".to_owned());
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let target = make_message(&conversation.conversation_id, "original");
    repo.insert_message(&target).await.unwrap();
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let operation_id = "public-edit-resubmit:v1:finished-orphan";
    let payload = format!(
        r#"{{"workflow":"edit-resubmit","target_message_id":"{}","content":"edited"}}"#,
        target.message_id
    );
    repo.claim_edit_resubmit_receipt_and_fence(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        &MessageId::new().into_string(),
        &payload,
        &target.message_id,
        initial.epoch,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();
    let reserved = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert!(
        repo.admit_reserved_edit_turn(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &payload,
            reserved.epoch,
            nomifun_common::now_ms() + 1,
        )
        .await
        .unwrap()
    );
    let admitted = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    expose_pre_008_running_exit_fixture(&db).await;
    sqlx::query(
        "UPDATE conversations SET status = 'finished' \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    assert_eq!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            "crashed while publishing edit completion",
            nomifun_common::now_ms() + 2,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    let repaired = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    assert_eq!(repaired.epoch, admitted.epoch + 1);
    assert_eq!(repaired.active_operation_id, None);
    let row = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert!(extra.get("_edit_resubmit_fence").is_none());
    assert_eq!(
        repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
}

#[tokio::test]
async fn admission_epoch_boundaries_finish_at_i64_max_without_overflow() {
    const MAX: i64 = i64::MAX;
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("max-normal-turn");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    sqlx::query(
        "UPDATE conversations SET admission_epoch = ? \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(MAX - 2)
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();
    let operation_id = "turn:max-final-generation";
    let payload = r#"{"content":"last representable generation"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        payload,
        MAX - 2,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap()
            .epoch,
        MAX - 1
    );
    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: payload.to_owned(),
                result_ok: true,
                result_text: Some("done".to_owned()),
                result_error: None,
            },
            nomifun_common::now_ms() + 1,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    assert_eq!(
        repo.get_turn_admission_state(USER_ID, &conversation.conversation_id)
            .await
            .unwrap()
            .epoch,
        MAX
    );
    let rejected_operation = "turn:past-max";
    assert!(matches!(
        repo.claim_turn_delivery_receipt_and_admit(
            USER_ID,
            &conversation.conversation_id,
            rejected_operation,
            r#"{"content":"must not overflow"}"#,
            MAX,
            nomifun_common::now_ms() + 2,
        )
        .await,
        Err(nomifun_db::DbError::Conflict(_))
    ));
    assert!(
        repo.get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            rejected_operation,
        )
        .await
        .unwrap()
        .is_none(),
        "rejected MAX admission must roll its receipt insert back"
    );
}

#[tokio::test]
async fn accepted_edit_resubmit_receipt_fences_rewind_and_truncate_crash_states_until_reset() {
    let (repo, _db) = setup().await;

    for phase in ["rewound", "truncated"] {
        let mut conversation = make_conversation(phase);
        conversation.conversation_id = repo.create(&conversation).await.unwrap();
        let operation_id = format!("edit-resubmit-crash:{phase}");
        let request_payload = format!(
            r#"{{"workflow":"edit-resubmit","target_message_id":"{}","content":"replacement"}}"#,
            MessageId::new().into_string()
        );
        let claim = repo
            .claim_delivery_receipt_once(
                USER_ID,
                &conversation.conversation_id,
                &operation_id,
                "turn",
                &request_payload,
                nomifun_common::now_ms(),
            )
            .await
            .unwrap();
        assert!(claim.claimed_new);

        let mut extra: serde_json::Value =
            serde_json::from_str(&conversation.extra).unwrap();
        extra["_edit_resubmit_fence"] = serde_json::json!({
            "operation_id": operation_id,
            "phase": phase,
        });
        repo.update(
            &conversation.conversation_id,
            &ConversationRowUpdate {
                extra: Some(extra.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let restarted_receiver = repo.clone();
        assert!(
            restarted_receiver
                .has_accepted_delivery_receipt_operation_prefix(
                    USER_ID,
                    &conversation.conversation_id,
                    "edit-resubmit-crash:",
                )
                .await
                .unwrap(),
            "a crash after {phase} must keep fresh sends fenced"
        );
        let replay = restarted_receiver
            .claim_delivery_receipt_once(
                USER_ID,
                &conversation.conversation_id,
                &operation_id,
                "turn",
                &request_payload,
                nomifun_common::now_ms() + 1,
            )
            .await
            .unwrap();
        assert!(
            !replay.claimed_new,
            "a restart after {phase} must absorb the edit workflow"
        );

        assert_eq!(
            restarted_receiver
                .reset_terminal_conversation(
                    USER_ID,
                    &conversation.conversation_id,
                    nomifun_common::now_ms() + 2,
                )
                .await
                .unwrap(),
            TurnLifecycleTransition::Committed
        );
        assert!(
            !restarted_receiver
                .has_accepted_delivery_receipt_operation_prefix(
                    USER_ID,
                    &conversation.conversation_id,
                    "edit-resubmit-crash:",
                )
                .await
                .unwrap()
        );
        let reset = restarted_receiver
            .get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap();
        let reset_extra: serde_json::Value =
            serde_json::from_str(&reset.extra).unwrap();
        assert!(reset_extra.get("_edit_resubmit_fence").is_none());
    }
}

#[tokio::test]
async fn running_authority_rejects_unkeyed_and_raw_reopen_but_allows_receipt_admission() {
    let (repo, db) = setup().await;

    let mut inserted_running = make_conversation("inserted-running-rejected");
    inserted_running.status = Some("running".to_owned());
    let insert_error = repo.create(&inserted_running).await.unwrap_err();
    assert!(
        insert_error
            .to_string()
            .contains("Conversation cannot be inserted Running"),
        "physical trigger must reject a forged Running aggregate: {insert_error}"
    );

    let mut conversation = make_conversation("turn-running-authority");
    conversation.status = Some("finished".to_owned());
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let now = nomifun_common::now_ms();

    let unkeyed_error = repo
        .mark_turn_running(USER_ID, &conversation.conversation_id, now)
        .await
        .unwrap_err();
    assert!(
        matches!(
            unkeyed_error,
            nomifun_db::DbError::Conflict(ref message)
                if message.contains("Unkeyed Conversation Running admission is forbidden")
        ),
        "legacy mark_turn_running must fail closed"
    );

    let generic_error = repo
        .update(
            &conversation.conversation_id,
            &ConversationRowUpdate {
                status: Some("running".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            generic_error,
            nomifun_db::DbError::Conflict(ref message)
                if message.contains("cannot change lifecycle status")
        ),
        "generic aggregate updates must not mint execution authority"
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );

    let null_status = sqlx::query(
        "UPDATE conversations SET status = NULL WHERE conversation_id = ?",
    )
        .bind(&conversation.conversation_id)
        .execute(db.pool())
        .await
    .unwrap_err();
    assert!(
        null_status
            .to_string()
            .contains("NOT NULL constraint failed: conversations.status"),
        "the published schema independently rejects a NULL lifecycle state: {null_status}"
    );

    let forged_operation = "turn:forged-finished-reopen";
    let forged_error = sqlx::query(
        "UPDATE conversations \
         SET status = 'running', active_turn_operation_id = ?, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    );
    let forged_error = forged_error
        .bind(forged_operation)
        .bind(&conversation.conversation_id)
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(
        forged_error
            .to_string()
            .contains("Conversation Running admission requires an exact accepted turn receipt"),
        "a Finished row cannot be reopened by forged raw SQL: {forged_error}"
    );

    let before_admission = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    let operation_id = "turn:receipt-backed-finished-reopen";
    let request_payload = r#"{"content":"new explicit turn"}"#;
    let claim = repo
        .claim_turn_delivery_receipt_and_admit(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            request_payload,
            before_admission.epoch,
            now + 3,
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    let admitted: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(admitted.0, "running");
    assert_eq!(admitted.1, before_admission.epoch + 1);
    assert_eq!(admitted.2.as_deref(), Some(operation_id));

    let owner_rewrite = sqlx::query(
        "UPDATE conversations SET active_turn_operation_id = ? \
         WHERE conversation_id = ?",
    )
    .bind("turn:forged-successor")
    .bind(&conversation.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        owner_rewrite
            .to_string()
            .contains("Conversation Running owner and epoch are immutable"),
        "a live Running owner cannot be rewritten in place: {owner_rewrite}"
    );

    let raw_pending_exit = sqlx::query(
        "UPDATE conversations SET status = 'pending' \
         WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        raw_pending_exit
            .to_string()
            .contains("Conversation Running exit requires completed turn receipts"),
        "Running cannot be reset without retiring its durable authority: {raw_pending_exit}"
    );

    let raw_unsettled_finish = sqlx::query(
        "UPDATE conversations \
         SET status = 'finished', active_turn_operation_id = NULL, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        raw_unsettled_finish
            .to_string()
            .contains("Conversation Running exit requires completed turn receipts"),
        "even a structurally valid exit must settle the accepted receipt first: {raw_unsettled_finish}"
    );

    let raw_delete = sqlx::query("DELETE FROM conversations WHERE conversation_id = ?")
        .bind(&conversation.conversation_id)
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(
        raw_delete
            .to_string()
            .contains("Conversation Running authority cannot be deleted"),
        "Running authority cannot disappear through raw deletion: {raw_delete}"
    );

    repo.complete_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        true,
        Some("completed exactly once"),
        None,
        now + 4,
    )
    .await
    .unwrap();
    let raw_null_exit = sqlx::query(
        "UPDATE conversations \
         SET status = NULL, active_turn_operation_id = NULL, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        raw_null_exit
            .to_string()
            .contains("Conversation Running exit requires completed turn receipts"),
        "a completed receipt must not make Running-to-NULL a valid terminal transition: {raw_null_exit}"
    );

    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: request_payload.to_owned(),
                result_ok: true,
                result_text: Some("completed exactly once".to_owned()),
                result_error: None,
            },
            now + 5,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    let finished: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(finished.0, "finished");
    assert_eq!(finished.1, admitted.1 + 1);
    assert_eq!(finished.2, None);
    let completed_receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed_receipt.status, "completed");
    assert_eq!(completed_receipt.result_ok, Some(true));
}

#[tokio::test]
async fn list_unsettled_turn_admissions_includes_running_and_finished_active_rows() {
    let (repo, db) = setup().await;
    let now = nomifun_common::now_ms();

    let mut ordinary_finished = make_conversation("ordinary-finished-not-unsettled");
    ordinary_finished.status = Some("finished".to_owned());
    ordinary_finished.conversation_id = repo.create(&ordinary_finished).await.unwrap();

    let mut running = make_conversation("enumerated-running");
    running.conversation_id = repo.create(&running).await.unwrap();
    let running_operation = "turn:enumerated-running";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &running.conversation_id,
        running_operation,
        r#"{"content":"running"}"#,
        0,
        now,
    )
    .await
    .unwrap();

    let mut partial = make_conversation("enumerated-finished-active");
    partial.conversation_id = repo.create(&partial).await.unwrap();
    let partial_operation = "turn:enumerated-finished-active";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &partial.conversation_id,
        partial_operation,
        r#"{"content":"partial"}"#,
        0,
        now + 1,
    )
    .await
    .unwrap();
    repo.complete_delivery_receipt(
        USER_ID,
        &partial.conversation_id,
        partial_operation,
        true,
        Some("receipt committed before aggregate cleanup"),
        None,
        now + 2,
    )
    .await
    .unwrap();
    expose_pre_008_running_exit_fixture(&db).await;
    sqlx::query(
        "UPDATE conversations SET status = 'finished' \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(&partial.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    let mut listed = Vec::new();
    let mut after = None;
    loop {
        let page = repo
            .list_unsettled_turn_admissions(after.as_deref(), 1)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        after = Some(page.last().unwrap().conversation.conversation_id.clone());
        listed.extend(page);
    }

    let mut expected_ids = vec![running.conversation_id.clone(), partial.conversation_id.clone()];
    expected_ids.sort();
    assert_eq!(
        listed
            .iter()
            .map(|admission| admission.conversation.conversation_id.clone())
            .collect::<Vec<_>>(),
        expected_ids,
        "the cross-owner recovery scan uses stable Conversation-ID ordering"
    );
    assert!(
        listed
            .iter()
            .all(|admission| admission.conversation.conversation_id
                != ordinary_finished.conversation_id)
    );

    let running_row = listed
        .iter()
        .find(|admission| admission.conversation.conversation_id == running.conversation_id)
        .unwrap();
    assert_eq!(running_row.conversation.status.as_deref(), Some("running"));
    assert_eq!(running_row.conversation.r#type, running.r#type);
    assert_eq!(running_row.conversation.user_id, USER_ID);
    assert_eq!(
        running_row.active_operation_id.as_deref(),
        Some(running_operation)
    );
    assert_eq!(running_row.admission_epoch, 1);

    let partial_row = listed
        .iter()
        .find(|admission| admission.conversation.conversation_id == partial.conversation_id)
        .unwrap();
    assert_eq!(partial_row.conversation.status.as_deref(), Some("finished"));
    assert_eq!(
        partial_row.active_operation_id.as_deref(),
        Some(partial_operation),
        "Finished plus active owner is an unsettled partial finalization"
    );
    assert_eq!(partial_row.admission_epoch, 1);
    assert!(
        repo.list_unsettled_turn_admissions(None, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn finalize_turn_atomically_finishes_conversation_and_receipt_and_replays() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("atomic-turn-finish");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:atomic-success";
    let request_payload = r#"{"content":"finish atomically"}"#;

    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        request_payload,
        0,
        accepted_at,
    )
    .await
    .unwrap();
    let completion = TurnReceiptCompletion {
        operation_id: operation_id.to_owned(),
        kind: "turn".to_owned(),
        request_payload: request_payload.to_owned(),
        result_ok: true,
        result_text: Some("done".to_owned()),
        result_error: None,
    };
    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &completion,
            accepted_at + 2,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );

    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(true));
    assert_eq!(receipt.result_text.as_deref(), Some("done"));
    assert_eq!(receipt.result_error, None);

    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &completion,
            accepted_at + 3,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::AlreadyApplied,
        "an exact completed result is an idempotent terminal replay"
    );
}

#[tokio::test]
async fn completed_old_receipt_cannot_finalize_a_new_running_turn_generation() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("old-receipt-new-running");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let operation_id = "old-turn:completed";
    let request_payload = r#"{"content":"old generation"}"#;
    let now = nomifun_common::now_ms();
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        "turn",
        request_payload,
        now,
    )
    .await
    .unwrap();
    repo.complete_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        true,
        Some("old result"),
        None,
        now + 1,
    )
    .await
    .unwrap();
    let new_operation_id = "turn:new-running-generation";
    let new_request_payload = r#"{"content":"new generation"}"#;
    let admission = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        new_operation_id,
        new_request_payload,
        admission.epoch,
        now + 2,
    )
    .await
    .unwrap();
    let stale = repo
        .finalize_turn(
            USER_ID,
            &conversation.conversation_id,
            Some(&TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: request_payload.to_owned(),
                result_ok: true,
                result_text: Some("old result".to_owned()),
                result_error: None,
            }),
            now + 3,
        )
        .await
        .unwrap();
    assert_eq!(stale, TurnLifecycleTransition::Stale);
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "a late finalizer for an old completed receipt must not finish the newer turn"
    );
}

#[tokio::test]
async fn finalize_turn_rejects_stale_pending_without_settling_receipt() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("stale-pending-finish");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:stale-pending";
    let request_payload = r#"{"content":"not started"}"#;
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        "turn",
        request_payload,
        accepted_at,
    )
    .await
    .unwrap();
    let completion = TurnReceiptCompletion {
        operation_id: operation_id.to_owned(),
        kind: "turn".to_owned(),
        request_payload: request_payload.to_owned(),
        result_ok: true,
        result_text: Some("must not commit".to_owned()),
        result_error: None,
    };

    assert_eq!(
        repo.finalize_turn(
            USER_ID,
            &conversation.conversation_id,
            Some(&completion),
            accepted_at + 1,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );
}

#[tokio::test]
async fn exact_finalize_adopts_terminal_receipt_and_missing_receipt_cannot_touch_running() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("receipt-finish-conflict");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:conflicting-result";
    let request_payload = r#"{"content":"terminal result"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        request_payload,
        0,
        accepted_at,
    )
    .await
    .unwrap();
    assert!(
        repo.complete_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            false,
            None,
            Some("original error"),
            accepted_at + 1,
        )
        .await
        .unwrap()
    );
    let conflicting = TurnReceiptCompletion {
        operation_id: operation_id.to_owned(),
        kind: "turn".to_owned(),
        request_payload: request_payload.to_owned(),
        result_ok: true,
        result_text: Some("different result".to_owned()),
        result_error: None,
    };
    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &conflicting,
            accepted_at + 3,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed,
        "an exact active generation adopts the receipt's already authoritative result"
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );

    let mut other = make_conversation("missing-receipt-running");
    other.conversation_id = repo.create(&other).await.unwrap();
    let active_operation = "turn:other-active";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &other.conversation_id,
        active_operation,
        r#"{"content":"other active"}"#,
        0,
        accepted_at + 4,
    )
    .await
    .unwrap();
    let missing = TurnReceiptCompletion {
        operation_id: "turn:not-claimed".to_owned(),
        kind: "turn".to_owned(),
        request_payload: request_payload.to_owned(),
        result_ok: true,
        result_text: Some("done".to_owned()),
        result_error: None,
    };
    assert!(matches!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &other.conversation_id,
            &missing,
            accepted_at + 5,
        )
        .await,
        Err(nomifun_db::DbError::NotFound(_))
    ));
    assert_eq!(
        repo.get(&other.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "receipt validation failures must roll back the Conversation transition"
    );
}

#[tokio::test]
async fn finalize_turn_rolls_back_receipt_if_conversation_finish_write_fails() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("atomic-finish-rollback");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:rollback";
    let request_payload = r#"{"content":"rollback"}"#;
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        request_payload,
        0,
        accepted_at,
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_test_conversation_finish \
         BEFORE UPDATE OF status ON conversations \
         WHEN NEW.status = 'finished' \
         BEGIN SELECT RAISE(ABORT, 'injected finish failure'); END",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let completion = TurnReceiptCompletion {
        operation_id: operation_id.to_owned(),
        kind: "turn".to_owned(),
        request_payload: request_payload.to_owned(),
        result_ok: true,
        result_text: Some("would finish".to_owned()),
        result_error: None,
    };

    assert!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &completion,
            accepted_at + 2,
        )
        .await
        .is_err()
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.status, "accepted",
        "receipt settlement and Conversation finalization share one transaction"
    );
    assert_eq!(receipt.result_ok, None);
}

#[tokio::test]
async fn finalize_orphaned_turn_atomically_fails_all_accepted_turn_receipts() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("orphaned-turn-success");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let turn_requests = [
        ("turn:orphaned:first", r#"{"content":"first"}"#),
        ("turn:orphaned:second", r#"{"content":"second"}"#),
    ];
    for (operation_id, request_payload) in turn_requests {
        repo.claim_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            "turn",
            request_payload,
            accepted_at,
        )
        .await
        .unwrap();
    }
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        "steer:orphaned:unrelated",
        "steer",
        r#"{"content":"steer"}"#,
        accepted_at,
    )
    .await
    .unwrap();
    expose_pre_008_running_admission_fixture(&db).await;
    sqlx::query(
        "UPDATE conversations \
         SET status = 'running', active_turn_operation_id = NULL, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();

    let wrong_owner = "0190f5fe-7c00-7a00-8000-000000000099";
    assert!(matches!(
        repo.finalize_orphaned_turn(
            wrong_owner,
            &conversation.conversation_id,
            "must not own this turn",
            accepted_at + 2,
        )
        .await,
        Err(nomifun_db::DbError::NotFound(_))
    ));
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );

    let reason = "runtime disappeared before durable turn completion";
    assert_eq!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            reason,
            accepted_at + 3,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    for (operation_id, _) in turn_requests {
        let receipt = repo
            .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.status, "completed");
        assert_eq!(receipt.result_ok, Some(false));
        assert_eq!(receipt.result_text, None);
        assert_eq!(receipt.result_error.as_deref(), Some(reason));
    }
    assert_eq!(
        repo.get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            "steer:orphaned:unrelated",
        )
        .await
        .unwrap()
        .unwrap()
        .status,
        "accepted",
        "orphan recovery must only settle turn receipts"
    );

    assert_eq!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            reason,
            accepted_at + 4,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::AlreadyApplied
    );
}

#[tokio::test]
async fn finalize_orphaned_turn_repairs_finished_with_historical_accepted_receipt() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("finished-orphan-repair");
    conversation.status = Some("finished".to_owned());
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:finished:historical";
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        "turn",
        r#"{"content":"historical split"}"#,
        accepted_at,
    )
    .await
    .unwrap();
    let reason = "recovered historical non-atomic turn completion";
    assert_eq!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            reason,
            accepted_at + 1,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(receipt.result_text, None);
    assert_eq!(receipt.result_error.as_deref(), Some(reason));

    assert_eq!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            reason,
            accepted_at + 2,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::AlreadyApplied
    );
}

#[tokio::test]
async fn finalize_orphaned_finished_receipt_repair_rolls_back_the_whole_batch() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("finished-orphan-repair-rollback");
    conversation.status = Some("finished".to_owned());
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    for operation_id in [
        "turn:finished:rollback:first",
        "turn:finished:rollback:second",
    ] {
        repo.claim_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            "turn",
            r#"{"content":"historical split"}"#,
            accepted_at,
        )
        .await
        .unwrap();
    }
    sqlx::query(
        "CREATE TRIGGER reject_second_historical_receipt_repair \
         BEFORE UPDATE OF status ON conversation_delivery_receipts \
         WHEN OLD.operation_id = 'turn:finished:rollback:second' \
              AND NEW.status = 'completed' \
         BEGIN SELECT RAISE(ABORT, 'injected receipt repair failure'); END",
    )
    .execute(db.pool())
    .await
    .unwrap();

    assert!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            "must roll back",
            accepted_at + 1,
        )
        .await
        .is_err()
    );
    for operation_id in [
        "turn:finished:rollback:first",
        "turn:finished:rollback:second",
    ] {
        let receipt = repo
            .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.status, "accepted",
            "one failing historical repair must roll back every receipt mutation"
        );
        assert_eq!(receipt.result_ok, None);
        assert_eq!(receipt.result_error, None);
    }
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );
}

#[tokio::test]
async fn finalize_orphaned_turn_rejects_pending_without_settling_receipts() {
    let (repo, _db) = setup().await;
    let mut conversation = make_conversation("orphaned-turn-pending");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:orphaned:pending";
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        "turn",
        r#"{"content":"pending"}"#,
        accepted_at,
    )
    .await
    .unwrap();

    assert_eq!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            "must stay pending",
            accepted_at + 1,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        repo.get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "accepted"
    );
}

#[tokio::test]
async fn finalize_orphaned_turn_rolls_back_receipts_when_finish_write_fails() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("orphaned-turn-rollback");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let accepted_at = nomifun_common::now_ms();
    let operation_id = "turn:orphaned:rollback";
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        r#"{"content":"rollback"}"#,
        0,
        accepted_at,
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_orphaned_conversation_finish \
         BEFORE UPDATE OF status ON conversations \
         WHEN NEW.status = 'finished' \
         BEGIN SELECT RAISE(ABORT, 'injected orphan finish failure'); END",
    )
    .execute(db.pool())
    .await
    .unwrap();

    assert!(
        repo.finalize_orphaned_turn(
            USER_ID,
            &conversation.conversation_id,
            "runtime disappeared",
            accepted_at + 2,
        )
        .await
        .is_err()
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    assert_eq!(receipt.result_ok, None);
    assert_eq!(receipt.result_error, None);
}

#[tokio::test]
async fn reset_terminal_conversation_clears_finished_aggregate_atomically() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("atomic-terminal-reset");
    conversation.status = Some("finished".to_owned());
    conversation.extra = serde_json::json!({
        "workspace": "/home/user/project",
        "custom_setting": "keep-me",
        "sessionKey": "openclaw-old-session",
        "session_key": "remote-old-session",
        "runtimeValidation": {"workspace": "/stale"},
        "runtime_validation": {"workspace": "/also-stale"},
        "acp_session_id": "legacy-acp-session",
        "acpSessionId": "legacy-camel-acp-session",
        "current_mode_id": "plan"
    })
    .to_string();
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    sqlx::query(
        "INSERT INTO acp_session (\
             conversation_id, agent_backend, agent_source, acp_session_id, \
             session_status, session_config, last_active_at\
         ) VALUES (?, 'claude', 'builtin', 'acp-session-before-reset', 'active', ?, 7)",
    )
    .bind(&conversation.conversation_id)
    .bind(
        serde_json::json!({
            "runtime": {
                "current_mode_id": "plan",
                "current_model_id": "model-kept",
                "context_usage": {"used": 999}
            },
            "pending_config_options": {"theme": "kept"}
        })
        .to_string(),
    )
    .execute(db.pool())
    .await
    .unwrap();
    let accepted_operation_id = "turn:reset-absorbs-accepted";
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        accepted_operation_id,
        "turn",
        r#"{"content":"must never replay"}"#,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();
    let completed_operation_id = "turn:reset-preserves-completed";
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        completed_operation_id,
        "turn",
        r#"{"content":"already complete"}"#,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();
    assert!(
        repo.complete_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            completed_operation_id,
            true,
            Some("original result"),
            None,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap()
    );
    let message = make_message(&conversation.conversation_id, "reset me");
    repo.insert_message(&message).await.unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;
    repo.upsert_artifact(&make_artifact(
        &conversation.conversation_id,
        &cron_job_id,
    ))
    .await
    .unwrap();
    let reset_at = nomifun_common::now_ms() + 100;

    assert_eq!(
        repo.reset_terminal_conversation(USER_ID, &conversation.conversation_id, reset_at)
            .await
            .unwrap(),
        TurnLifecycleTransition::Committed
    );
    let reset = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reset.status.as_deref(), Some("pending"));
    assert_eq!(reset.updated_at, reset_at);
    let reset_extra: serde_json::Value = serde_json::from_str(&reset.extra).unwrap();
    for removed in [
        "sessionKey",
        "session_key",
        "runtimeValidation",
        "runtime_validation",
        "acp_session_id",
        "acpSessionId",
    ] {
        assert!(
            reset_extra.get(removed).is_none(),
            "runtime resume key {removed} must be removed by the reset transaction"
        );
    }
    assert_eq!(
        reset_extra.get("workspace").and_then(serde_json::Value::as_str),
        Some("/home/user/project"),
        "unrelated workspace configuration must be preserved"
    );
    assert_eq!(
        reset_extra
            .get("custom_setting")
            .and_then(serde_json::Value::as_str),
        Some("keep-me")
    );
    assert_eq!(
        reset_extra
            .get("current_mode_id")
            .and_then(serde_json::Value::as_str),
        Some("plan"),
        "runtime preferences are configuration, not a resumable session identity"
    );
    let absorbed = repo
        .get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            accepted_operation_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(absorbed.status, "completed");
    assert_eq!(absorbed.result_ok, Some(false));
    assert_eq!(absorbed.result_text, None);
    assert_eq!(absorbed.result_error.as_deref(), Some("conversation reset"));
    let already_completed = repo
        .get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            completed_operation_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(already_completed.status, "completed");
    assert_eq!(already_completed.result_ok, Some(true));
    assert_eq!(already_completed.result_text.as_deref(), Some("original result"));
    assert_eq!(already_completed.result_error, None);
    let (acp_session_id, session_status, session_config, last_active_at):
        (Option<String>, String, String, Option<i64>) = sqlx::query_as(
            "SELECT acp_session_id, session_status, session_config, last_active_at \
             FROM acp_session WHERE conversation_id = ?",
        )
        .bind(&conversation.conversation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(acp_session_id, None);
    assert_eq!(session_status, "idle");
    assert_eq!(last_active_at, Some(reset_at));
    let session_config: serde_json::Value = serde_json::from_str(&session_config).unwrap();
    assert!(
        session_config
            .pointer("/runtime/context_usage")
            .is_none(),
        "reset must remove only cached ACP context usage"
    );
    assert_eq!(
        session_config
            .pointer("/runtime/current_mode_id")
            .and_then(serde_json::Value::as_str),
        Some("plan")
    );
    assert_eq!(
        session_config
            .pointer("/runtime/current_model_id")
            .and_then(serde_json::Value::as_str),
        Some("model-kept")
    );
    assert_eq!(
        session_config
            .pointer("/pending_config_options/theme")
            .and_then(serde_json::Value::as_str),
        Some("kept")
    );
    assert_eq!(
        repo.get_messages(&conversation.conversation_id, 1, 10, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        0
    );
    assert!(
        repo.list_artifacts(&conversation.conversation_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reset_terminal_conversation_recovers_pending_with_legacy_history() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("legacy-pending-reset");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    repo.insert_message(&make_message(
        &conversation.conversation_id,
        "legacy pending history",
    ))
    .await
    .unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;
    repo.upsert_artifact(&make_artifact(
        &conversation.conversation_id,
        &cron_job_id,
    ))
    .await
    .unwrap();

    assert_eq!(
        repo.reset_terminal_conversation(
            USER_ID,
            &conversation.conversation_id,
            nomifun_common::now_ms() + 100,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        repo.get_messages(&conversation.conversation_id, 1, 10, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        0
    );
    assert!(
        repo.list_artifacts(&conversation.conversation_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reset_terminal_conversation_rejects_wrong_owner_and_running_state() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("running-reset-rejected");
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    repo.insert_message(&make_message(
        &conversation.conversation_id,
        "active turn history",
    ))
    .await
    .unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;
    repo.upsert_artifact(&make_artifact(
        &conversation.conversation_id,
        &cron_job_id,
    ))
    .await
    .unwrap();
    repo.claim_turn_delivery_receipt_and_admit(
        USER_ID,
        &conversation.conversation_id,
        "turn:reset-running-state",
        r#"{"content":"active turn"}"#,
        0,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();

    let wrong_owner = "0190f5fe-7c00-7a00-8000-000000000099";
    assert!(matches!(
        repo.reset_terminal_conversation(
            wrong_owner,
            &conversation.conversation_id,
            nomifun_common::now_ms(),
        )
        .await,
        Err(nomifun_db::DbError::NotFound(_))
    ));
    assert_eq!(
        repo.reset_terminal_conversation(
            USER_ID,
            &conversation.conversation_id,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );
    assert_eq!(
        repo.get(&conversation.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running")
    );
    assert_eq!(
        repo.get_messages(&conversation.conversation_id, 1, 10, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        repo.list_artifacts(&conversation.conversation_id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn reset_terminal_conversation_rolls_back_deletes_when_status_write_fails() {
    let (repo, db) = setup().await;
    let mut conversation = make_conversation("terminal-reset-rollback");
    conversation.status = Some("finished".to_owned());
    conversation.extra = serde_json::json!({
        "workspace": "/home/user/project",
        "sessionKey": "must-survive-rollback",
        "runtimeValidation": {"generation": 7}
    })
    .to_string();
    let original_extra: serde_json::Value = serde_json::from_str(&conversation.extra).unwrap();
    conversation.conversation_id = repo.create(&conversation).await.unwrap();
    let original_acp_config = serde_json::json!({
        "runtime": {
            "current_mode_id": "code",
            "context_usage": {"used": 42}
        }
    })
    .to_string();
    sqlx::query(
        "INSERT INTO acp_session (\
             conversation_id, agent_backend, agent_source, acp_session_id, \
             session_status, session_config, last_active_at\
         ) VALUES (?, 'claude', 'builtin', 'acp-session-must-rollback', 'active', ?, 11)",
    )
    .bind(&conversation.conversation_id)
    .bind(&original_acp_config)
    .execute(db.pool())
    .await
    .unwrap();
    let accepted_operation_id = "turn:reset-rollback-receipt";
    repo.claim_delivery_receipt(
        USER_ID,
        &conversation.conversation_id,
        accepted_operation_id,
        "turn",
        r#"{"content":"must remain accepted"}"#,
        nomifun_common::now_ms(),
    )
    .await
    .unwrap();
    repo.insert_message(&make_message(
        &conversation.conversation_id,
        "must survive rollback",
    ))
    .await
    .unwrap();
    let cron_job_id = seed_cron_job(db.pool()).await;
    repo.upsert_artifact(&make_artifact(
        &conversation.conversation_id,
        &cron_job_id,
    ))
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_terminal_conversation_reset \
         BEFORE UPDATE OF status ON conversations \
         WHEN NEW.status = 'pending' \
         BEGIN SELECT RAISE(ABORT, 'injected reset failure'); END",
    )
    .execute(db.pool())
    .await
    .unwrap();

    assert!(
        repo.reset_terminal_conversation(
            USER_ID,
            &conversation.conversation_id,
            nomifun_common::now_ms() + 100,
        )
        .await
        .is_err()
    );
    let after_failure = repo
        .get(&conversation.conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_failure.status.as_deref(), Some("finished"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&after_failure.extra).unwrap(),
        original_extra,
        "resume-key stripping must roll back with transcript/artifact/status reset"
    );
    let receipt_after_failure = repo
        .get_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            accepted_operation_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt_after_failure.status, "accepted",
        "accepted receipt settlement must roll back with aggregate reset"
    );
    assert_eq!(receipt_after_failure.result_ok, None);
    assert_eq!(receipt_after_failure.result_error, None);
    let (rolled_back_session_id, rolled_back_status, rolled_back_config, rolled_back_active_at):
        (Option<String>, String, String, Option<i64>) = sqlx::query_as(
            "SELECT acp_session_id, session_status, session_config, last_active_at \
             FROM acp_session WHERE conversation_id = ?",
        )
        .bind(&conversation.conversation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        rolled_back_session_id.as_deref(),
        Some("acp-session-must-rollback")
    );
    assert_eq!(rolled_back_status, "active");
    assert_eq!(rolled_back_config, original_acp_config);
    assert_eq!(rolled_back_active_at, Some(11));
    assert_eq!(
        repo.get_messages(&conversation.conversation_id, 1, 10, SortOrder::Asc)
            .await
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        repo.list_artifacts(&conversation.conversation_id)
            .await
            .unwrap()
            .len(),
        1,
        "message and artifact deletions must roll back with the status write"
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

#[tokio::test]
async fn abandoned_candidate_owned_turn_is_settled_with_its_exact_generation() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("candidate-abandon");
    repo.create(&conversation).await.unwrap();
    let operation_id = "public-turn:v1:owner:conversation:candidate-abandon";
    let candidate_message_id = MessageId::new().into_string();
    let request_payload = r#"{"content":"candidate abandon"}"#;
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();

    let claim = repo
        .claim_turn_delivery_receipt_and_admit_with_candidate(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            request_payload,
            initial.epoch,
            100,
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    assert_eq!(claim.receipt.message_id, candidate_message_id);

    let transition = repo
        .abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            request_payload,
            initial.epoch + 1,
            "request future dropped",
            200,
        )
        .await
        .unwrap();
    assert_eq!(transition, TurnLifecycleTransition::Committed);

    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(receipt.result_error.as_deref(), Some("request future dropped"));
    let (status, epoch, active): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "finished");
    assert_eq!(epoch, initial.epoch + 2);
    assert_eq!(active, None);

    assert_eq!(
        repo.abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            request_payload,
            initial.epoch + 1,
            "request future dropped",
            300,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::AlreadyApplied
    );
}

#[tokio::test]
async fn cancelled_insert_loser_cannot_abandon_the_candidate_winner() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("candidate-loser");
    repo.create(&conversation).await.unwrap();
    let operation_id = "public-turn:v1:owner:conversation:candidate-loser";
    let winner_candidate = MessageId::new().into_string();
    let loser_candidate = MessageId::new().into_string();
    let request_payload = r#"{"content":"one winner"}"#;
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();

    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        &winner_candidate,
        request_payload,
        initial.epoch,
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        repo.abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &loser_candidate,
            request_payload,
            initial.epoch + 1,
            "loser future dropped",
            200,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );

    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.message_id, winner_candidate);
    assert_eq!(receipt.status, "accepted");
    let (status, epoch, active): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "running");
    assert_eq!(epoch, initial.epoch + 1);
    assert_eq!(active.as_deref(), Some(operation_id));
}

#[tokio::test]
async fn autowork_receipt_payload_authority_is_exactly_bound_before_persistence() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("autowork-payload-authority-binding");
    repo.create(&conversation).await.unwrap();

    let requirement_id = RequirementId::new().into_string();
    sqlx::query(
        "INSERT INTO requirements (\
            requirement_id, display_no, title, tag, status, created_by, created_at, updated_at\
         ) VALUES (?, 900001, 'authority binding', 'authority-binding', 'pending', ?, 100, 100)",
    )
    .bind(&requirement_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();
    let requirement_repo = SqliteRequirementRepository::new(db.pool().clone());
    let claim = requirement_repo
        .claim_next_for_runner(
            "authority-binding",
            Some(&conversation.conversation_id),
            None,
            60_000,
            100,
        )
        .await
        .unwrap()
        .expect("pending Requirement must be claimed");
    let claim_token = claim
        .row
        .claim_token
        .clone()
        .expect("active claim has a capability");
    let authority = RequirementConversationTurnAuthority {
        requirement_id: requirement_id.clone(),
        claim_generation: claim.row.claim_generation,
        claim_token: claim_token.clone(),
    };
    let claim_token_sha256 = format!("{:x}", Sha256::digest(claim_token.as_bytes()));
    let candidate = MessageId::new().into_string();

    let mismatched_requirement_payload = serde_json::json!({
        "delivery": {"content": "work"},
        "autowork_authority": {
            "requirement_id": RequirementId::new().into_string(),
            "claim_generation": authority.claim_generation,
            "claim_token_sha256": &claim_token_sha256,
        },
    })
    .to_string();
    let requirement_error = repo
        .claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
            USER_ID,
            &conversation.conversation_id,
            "autowork:payload-wrong-requirement",
            &candidate,
            &mismatched_requirement_payload,
            &authority,
            0,
            200,
        )
        .await
        .unwrap_err();
    assert!(
        requirement_error
            .to_string()
            .contains("payload authority does not match"),
        "a receipt cannot persist a different Requirement identity: {requirement_error}"
    );

    let string_generation_payload = serde_json::json!({
        "delivery": {"content": "work"},
        "autowork_authority": {
            "requirement_id": &authority.requirement_id,
            "claim_generation": authority.claim_generation.to_string(),
            "claim_token_sha256": &claim_token_sha256,
        },
    })
    .to_string();
    let type_error = repo
        .claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
            USER_ID,
            &conversation.conversation_id,
            "autowork:payload-string-generation",
            &candidate,
            &string_generation_payload,
            &authority,
            0,
            201,
        )
        .await
        .unwrap_err();
    assert!(
        type_error
            .to_string()
            .contains("payload authority does not match"),
        "claim_generation must be a JSON integer, not an equivalent string: {type_error}"
    );

    let mismatched_digest_payload = serde_json::json!({
        "delivery": {"content": "work"},
        "autowork_authority": {
            "requirement_id": &authority.requirement_id,
            "claim_generation": authority.claim_generation,
            "claim_token_sha256": "0".repeat(64),
        },
    })
    .to_string();
    let digest_error = repo
        .claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
            USER_ID,
            &conversation.conversation_id,
            "autowork:payload-wrong-capability",
            &candidate,
            &mismatched_digest_payload,
            &authority,
            0,
            202,
        )
        .await
        .unwrap_err();
    assert!(
        digest_error
            .to_string()
            .contains("payload authority does not match"),
        "the persisted digest must bind the exact admitted capability: {digest_error}"
    );

    let persisted_after_rejections: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_delivery_receipts \
         WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        persisted_after_rejections, 0,
        "authority mismatch must roll back before receipt persistence"
    );
    let untouched: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(untouched, ("pending".to_owned(), 0, None));

    let exact_payload = serde_json::json!({
        "delivery": {"content": "work"},
        "autowork_authority": {
            "requirement_id": &authority.requirement_id,
            "claim_generation": authority.claim_generation,
            "claim_token_sha256": &claim_token_sha256,
        },
    })
    .to_string();
    let exact = repo
        .claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
            USER_ID,
            &conversation.conversation_id,
            "autowork:payload-exact-authority",
            &candidate,
            &exact_payload,
            &authority,
            0,
            203,
        )
        .await
        .unwrap();
    assert!(exact.claimed_new);
    assert_eq!(exact.receipt.request_payload, exact_payload);
    assert!(
        !exact.receipt.request_payload.contains(&claim_token),
        "the opaque capability itself must never be persisted"
    );
}

#[tokio::test]
async fn conversation_delivery_receipt_identity_and_retention_are_physical_invariants() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("receipt-physical-invariants");
    repo.create(&conversation).await.unwrap();
    let operation_id = "turn:receipt-physical-invariants";
    let candidate_message_id = MessageId::new().into_string();
    let request_payload = r#"{"content":"immutable replay evidence"}"#;
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        &candidate_message_id,
        request_payload,
        0,
        100,
    )
    .await
    .unwrap();

    let identity_error = sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET message_id = ?, request_payload = '{\"content\":\"rewritten\"}' \
         WHERE operation_id = ?",
    )
    .bind(MessageId::new().into_string())
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        identity_error
            .to_string()
            .contains("Conversation delivery receipt identity is immutable"),
        "scope, payload, and candidate rewrites must be rejected by SQLite: {identity_error}"
    );

    let delete_error =
        sqlx::query("DELETE FROM conversation_delivery_receipts WHERE operation_id = ?")
            .bind(operation_id)
            .execute(db.pool())
            .await
            .unwrap_err();
    assert!(
        delete_error
            .to_string()
            .contains("Conversation delivery receipts are retained indefinitely"),
        "terminal replay evidence must not be physically deletable: {delete_error}"
    );

    let accepted_shape_error = sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET result_ok = 1 \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        accepted_shape_error
            .to_string()
            .contains("lifecycle is absorbing"),
        "accepted receipts cannot carry a terminal outcome: {accepted_shape_error}"
    );

    let wrong_terminal_shape = sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET status = 'completed', result_ok = NULL, result_text = 'partial output', \
             result_error = 'failed after partial output', completed_at = 200 \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        wrong_terminal_shape
            .to_string()
            .contains("lifecycle is absorbing"),
        "accepted-to-completed is legal only with a valid terminal shape: {wrong_terminal_shape}"
    );
    let invalid_completed_insert = sqlx::query(
        "INSERT INTO conversation_delivery_receipts (\
            operation_id, message_id, conversation_id, user_id, kind, request_payload, \
            status, result_ok, result_text, result_error, created_at, updated_at, completed_at\
         ) VALUES (\
            'turn:invalid-completed-insert', ?, ?, ?, 'turn', '{}', \
            'completed', NULL, 'partial output', 'failed', 100, 100, 200\
         )",
    )
    .bind(MessageId::new().into_string())
    .bind(&conversation.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        invalid_completed_insert
            .to_string()
            .contains("invalid lifecycle shape"),
        "completed inserts must carry one valid immutable outcome: {invalid_completed_insert}"
    );

    assert!(
        repo.complete_delivery_receipt(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            true,
            Some("completed"),
            None,
            200,
        )
        .await
        .unwrap(),
        "the legal accepted-to-completed outcome transition remains available"
    );
    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: "turn".to_owned(),
                request_payload: request_payload.to_owned(),
                result_ok: true,
                result_text: Some("completed".to_owned()),
                result_error: None,
            },
            201,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed
    );

    let rollback_error = sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET status = 'accepted', result_ok = NULL, result_text = NULL, \
             result_error = NULL, completed_at = NULL \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        rollback_error
            .to_string()
            .contains("lifecycle is absorbing"),
        "completed is absorbing even when raw SQL tries to restore accepted shape: {rollback_error}"
    );

    let rewrite_error = sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET result_text = 'rewritten terminal result' \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        rewrite_error
            .to_string()
            .contains("terminal outcomes are immutable"),
        "a completed receipt outcome cannot be rewritten: {rewrite_error}"
    );

    let old_operation_reopen = sqlx::query(
        "UPDATE conversations \
         SET status = 'running', active_turn_operation_id = ?, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    )
    .bind(operation_id)
    .bind(&conversation.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        old_operation_reopen
            .to_string()
            .contains("Running admission requires an exact accepted turn receipt"),
        "the immutable completed receipt cannot authorize a later Running generation: {old_operation_reopen}"
    );

    sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET projected_conversation_id = NULL, projected_message_id = NULL, \
             updated_at = updated_at + 1 \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap();

    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.message_id, candidate_message_id);
    assert_eq!(receipt.request_payload, request_payload);
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(true));
    assert!(receipt.projected_conversation_id.is_none());
    assert!(receipt.projected_message_id.is_none());

    let failed_conversation = make_conversation("receipt-failed-with-partial-output");
    repo.create(&failed_conversation).await.unwrap();
    let failed_operation = "turn:receipt-failed-with-partial-output";
    let failed_payload = r#"{"content":"produce partial output then fail"}"#;
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &failed_conversation.conversation_id,
        failed_operation,
        &MessageId::new().into_string(),
        failed_payload,
        0,
        300,
    )
    .await
    .unwrap();
    assert_eq!(
        repo.finalize_exact_turn_operation(
            USER_ID,
            &failed_conversation.conversation_id,
            &TurnReceiptCompletion {
                operation_id: failed_operation.to_owned(),
                kind: "turn".to_owned(),
                request_payload: failed_payload.to_owned(),
                result_ok: false,
                result_text: Some("usable partial final text".to_owned()),
                result_error: Some("provider disconnected after partial output".to_owned()),
            },
            400,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Committed,
        "a failed/cancelled turn may durably retain partial final text and its error"
    );
    let failed_receipt = repo
        .get_delivery_receipt(
            USER_ID,
            &failed_conversation.conversation_id,
            failed_operation,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_receipt.status, "completed");
    assert_eq!(failed_receipt.result_ok, Some(false));
    assert_eq!(
        failed_receipt.result_text.as_deref(),
        Some("usable partial final text")
    );
    assert_eq!(
        failed_receipt.result_error.as_deref(),
        Some("provider disconnected after partial output")
    );
    let failed_aggregate: (String, Option<String>) = sqlx::query_as(
        "SELECT status, active_turn_operation_id FROM conversations \
         WHERE conversation_id = ?",
    )
    .bind(&failed_conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(failed_aggregate, ("finished".to_owned(), None));
}

#[tokio::test]
async fn conversation_turn_authority_triggers_reject_null_epochs_without_three_value_bypass() {
    let (repo, db) = setup().await;

    let admission = make_conversation("null-admission-epoch");
    repo.create(&admission).await.unwrap();
    let admission_operation = "turn:null-admission-epoch";
    sqlx::query(
        "INSERT INTO conversation_delivery_receipts (\
            operation_id, message_id, conversation_id, projected_conversation_id, \
            user_id, kind, request_payload, status, created_at, updated_at\
         ) VALUES (?, ?, ?, ?, ?, 'turn', '{\"content\":\"null admission\"}', \
                   'accepted', 100, 100)",
    )
    .bind(admission_operation)
    .bind(MessageId::new().into_string())
    .bind(&admission.conversation_id)
    .bind(&admission.conversation_id)
    .bind(USER_ID)
    .execute(db.pool())
    .await
    .unwrap();
    let admission_error = sqlx::query(
        "UPDATE conversations \
         SET status = 'running', active_turn_operation_id = ?, admission_epoch = NULL \
         WHERE conversation_id = ?",
    )
    .bind(admission_operation)
    .bind(&admission.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        admission_error.to_string().contains(
            "Conversation Running admission requires an exact accepted turn receipt and next epoch"
        ),
        "the admission trigger itself must reject NULL instead of falling through to a generic NOT NULL error: {admission_error}"
    );

    let active = make_conversation("null-active-epoch");
    repo.create(&active).await.unwrap();
    let active_operation = "turn:null-active-epoch";
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &active.conversation_id,
        active_operation,
        &MessageId::new().into_string(),
        r#"{"content":"null active"}"#,
        0,
        200,
    )
    .await
    .unwrap();
    let owner_error = sqlx::query(
        "UPDATE conversations SET admission_epoch = NULL WHERE conversation_id = ?",
    )
    .bind(&active.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        owner_error
            .to_string()
            .contains("Conversation Running owner and epoch are immutable"),
        "the active-owner trigger itself must reject NULL: {owner_error}"
    );

    let exiting = make_conversation("null-exit-epoch");
    repo.create(&exiting).await.unwrap();
    let exit_operation = "turn:null-exit-epoch";
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &exiting.conversation_id,
        exit_operation,
        &MessageId::new().into_string(),
        r#"{"content":"null exit"}"#,
        0,
        300,
    )
    .await
    .unwrap();
    let exit_error = sqlx::query(
        "UPDATE conversations \
         SET status = 'finished', active_turn_operation_id = NULL, admission_epoch = NULL \
         WHERE conversation_id = ?",
    )
    .bind(&exiting.conversation_id)
    .execute(db.pool())
    .await
    .unwrap_err();
    assert!(
        exit_error.to_string().contains(
            "Conversation Running exit requires completed turn receipts, Finished state, cleared owner, and next epoch"
        ),
        "the exit trigger itself must reject NULL instead of relying on a later column constraint: {exit_error}"
    );
}

#[tokio::test]
async fn late_abandon_of_a_preserves_external_a_result_and_active_b() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("candidate-a-versus-b");
    repo.create(&conversation).await.unwrap();
    let operation_a = "public-turn:v1:owner:conversation:candidate-a";
    let operation_b = "public-turn:v1:owner:conversation:candidate-b";
    let candidate_a = MessageId::new().into_string();
    let candidate_b = MessageId::new().into_string();
    let payload_a = r#"{"content":"A"}"#;
    let payload_b = r#"{"content":"B"}"#;
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &conversation.conversation_id,
        operation_a,
        &candidate_a,
        payload_a,
        initial.epoch,
        100,
    )
    .await
    .unwrap();
    repo.finalize_exact_turn_operation(
        USER_ID,
        &conversation.conversation_id,
        &TurnReceiptCompletion {
            operation_id: operation_a.to_owned(),
            kind: "turn".to_owned(),
            request_payload: payload_a.to_owned(),
            result_ok: true,
            result_text: Some("external A result".to_owned()),
            result_error: None,
        },
        200,
    )
    .await
    .unwrap();
    let after_a = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &conversation.conversation_id,
        operation_b,
        &candidate_b,
        payload_b,
        after_a.epoch,
        300,
    )
    .await
    .unwrap();

    assert_eq!(
        repo.abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_a,
            &candidate_a,
            payload_a,
            initial.epoch + 1,
            "late A request drop",
            400,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );
    let receipt_a = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt_a.result_ok, Some(true));
    assert_eq!(receipt_a.result_text.as_deref(), Some("external A result"));
    let receipt_b = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt_b.status, "accepted");
    let (status, epoch, active): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "running");
    assert_eq!(epoch, after_a.epoch + 1);
    assert_eq!(active.as_deref(), Some(operation_b));
}

#[tokio::test]
async fn abandoned_uncommitted_candidate_without_receipt_is_stale() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("candidate-missing");
    repo.create(&conversation).await.unwrap();
    assert_eq!(
        repo.abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            "public-turn:v1:owner:conversation:missing",
            &MessageId::new().into_string(),
            r#"{"content":"missing"}"#,
            1,
            "request future dropped",
            100,
        )
        .await
        .unwrap(),
        TurnLifecycleTransition::Stale
    );
    let (status, epoch, active): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(epoch, 0);
    assert_eq!(active, None);
}

#[tokio::test]
async fn missing_receipt_for_exact_active_admission_remains_quarantined() {
    let (repo, db) = setup().await;
    expose_missing_receipt_corruption_fixture(&db).await;
    let conversation = make_conversation("candidate-active-missing");
    repo.create(&conversation).await.unwrap();
    let operation_id = "public-turn:v1:owner:conversation:candidate-active-missing";
    let candidate_message_id = MessageId::new().into_string();
    let request_payload = r#"{"content":"active missing"}"#;
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        &candidate_message_id,
        request_payload,
        initial.epoch,
        100,
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM conversation_delivery_receipts WHERE operation_id = ?")
        .bind(operation_id)
        .execute(db.pool())
        .await
        .unwrap();

    let error = repo
        .abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            request_payload,
            initial.epoch + 1,
            "request future dropped",
            200,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        nomifun_db::DbError::Conflict(message)
            if message.contains("no immutable exact receipt proof")
    ));
    let (status, epoch, active): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "running");
    assert_eq!(epoch, initial.epoch + 1);
    assert_eq!(active.as_deref(), Some(operation_id));
}

#[tokio::test]
async fn abandoned_admission_with_wrong_payload_epoch_or_result_shape_remains_quarantined() {
    let (repo, db) = setup().await;
    let conversation = make_conversation("candidate-corrupt");
    repo.create(&conversation).await.unwrap();
    let operation_id = "public-turn:v1:owner:conversation:candidate-corrupt";
    let candidate_message_id = MessageId::new().into_string();
    let request_payload = r#"{"content":"immutable"}"#;
    let initial = repo
        .get_turn_admission_state(USER_ID, &conversation.conversation_id)
        .await
        .unwrap();
    repo.claim_turn_delivery_receipt_and_admit_with_candidate(
        USER_ID,
        &conversation.conversation_id,
        operation_id,
        &candidate_message_id,
        request_payload,
        initial.epoch,
        100,
    )
    .await
    .unwrap();

    let payload_error = repo
        .abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            r#"{"content":"drifted"}"#,
            initial.epoch + 1,
            "request future dropped",
            200,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        payload_error,
        nomifun_db::DbError::Conflict(message)
            if message.contains("receipt identity is invalid")
    ));

    let epoch_error = repo
        .abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            request_payload,
            initial.epoch + 2,
            "request future dropped",
            300,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        epoch_error,
        nomifun_db::DbError::Conflict(message)
            if message.contains("does not match its exact epoch")
    ));

    expose_pre_012_receipt_lifecycle_corruption_fixture(&db).await;
    sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET status = 'completed', result_ok = NULL, completed_at = 350 \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(db.pool())
    .await
    .unwrap();
    let result_shape_error = repo
        .abandon_exact_turn_admission(
            USER_ID,
            &conversation.conversation_id,
            operation_id,
            &candidate_message_id,
            request_payload,
            initial.epoch + 1,
            "request future dropped",
            400,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        result_shape_error,
        nomifun_db::DbError::Conflict(message)
            if message.contains("receipt identity is invalid")
    ));

    let receipt = repo
        .get_delivery_receipt(USER_ID, &conversation.conversation_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, None);
    let (status, epoch, active): (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation.conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(status, "running");
    assert_eq!(epoch, initial.epoch + 1);
    assert_eq!(active.as_deref(), Some(operation_id));
}
