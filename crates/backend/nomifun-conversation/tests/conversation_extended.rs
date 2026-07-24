use std::sync::Arc;

use nomifun_ai_agent::AgentRuntimeRegistry;
use nomifun_api_types::{
    CloneConversationRequest, CreateConversationRequest, ListMessagesQuery, SearchMessagesQuery, WebSocketMessage,
};
use nomifun_common::{AgentKillReason, AppError, ConversationStatus, TimestampMs, now_ms};
use nomifun_conversation::ConversationService;
use nomifun_conversation::skill_resolver::SkillResolver;
use nomifun_db::models::MessageRow;
use nomifun_db::{IProviderRepository, IConversationRepository, SqliteConversationRepository};
use nomifun_realtime::UserEventSink;
use serde_json::json;
use std::sync::Mutex;

// ── Test infrastructure ────────────────────────────────────────────

struct TestBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl TestBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(vec![]),
        }
    }
}

impl UserEventSink for TestBroadcaster {
    fn send_to_user(&self, _user_id: &str, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct NoopAgentRuntimeRegistry;

#[async_trait::async_trait]
impl AgentRuntimeRegistry for NoopAgentRuntimeRegistry {
    fn get_runtime(&self, _: &str) -> Option<nomifun_ai_agent::AgentRuntimeHandle> {
        None
    }
    async fn get_or_create_runtime(
        &self,
        _: &str,
        _: nomifun_ai_agent::types::AgentRuntimeBuildOptions,
    ) -> Result<nomifun_ai_agent::AgentRuntimeHandle, AppError> {
        Err(AppError::Internal("noop".into()))
    }
    fn terminate(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
    fn terminate_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    fn terminate_all(&self) {}
    fn active_runtime_count(&self) -> usize {
        0
    }
    fn collect_idle_runtimes(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct EmptySkillResolver;

#[async_trait::async_trait]
impl SkillResolver for EmptySkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<nomifun_extension::ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &std::path::Path,
        _rel_dirs: &[&str],
        _skills: &[nomifun_extension::ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

async fn setup() -> (
    ConversationService,
    Arc<SqliteConversationRepository>,
    Arc<TestBroadcaster>,
) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let provider_repo = nomifun_db::SqliteProviderRepository::new(db.pool().clone());
    provider_repo.create(nomifun_db::CreateProviderParams { provider_id: Some("0190f5fe-7c00-7a00-8000-000000000001"), platform: "openai", name: "test", base_url: "https://example.invalid", api_key_encrypted: "", models: "[\"m1\",\"gpt-4o\",\"claude-sonnet-4-20250514\"]", enabled: true, capabilities: "[]", model_context_limits: None, model_protocols: None, model_descriptions: None, model_enabled: None, model_health: None, bedrock_config: None, is_full_url: false, sort_order: Some(0) }).await.unwrap();
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
        Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
        Arc::new(nomifun_db::SqliteAcpSessionRepository::new(db.pool().clone()));
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(NoopAgentRuntimeRegistry);
    let svc = ConversationService::new(
        Arc::<str>::from(USER_ID),
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(EmptySkillResolver),
        runtime_registry,
        repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
        Arc::new(nomifun_conversation::NoExecutionConversationBoundary),
    );
    (svc, repo, broadcaster)
}

const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const TEST_ACP_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000101";

async fn init_database_memory() -> Result<nomifun_db::Database, nomifun_db::DbError> {
    nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(USER_ID.to_owned()).expect("canonical fixture owner"),
    )
    .await
}

fn make_create_req() -> CreateConversationRequest {
    serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "agent_id": TEST_ACP_AGENT_ID,
            "workspace": "/home/user/project"
        }
    }))
    .unwrap()
}

fn make_message(conv_id: String, content: &str, offset_ms: i64) -> MessageRow {
    let message_id = nomifun_common::MessageId::new().into_string();
    MessageRow {
        id: 0,
        message_id: message_id.clone(),
        conversation_id: conv_id,
        msg_id: Some(message_id),
        r#type: "text".to_string(),
        content: format!(r#"{{"content":"{content}"}}"#),
        position: Some("right".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now_ms() + offset_ms,
    }
}

fn make_acp_tool_message(conv_id: String, id: &str, output: &str, offset_ms: i64) -> MessageRow {
    MessageRow {
        id: 0,
        message_id: id.to_string(),
        conversation_id: conv_id,
        msg_id: Some(id.to_string()),
        r#type: "acp_tool_call".to_string(),
        content: json!({
            "session_id": "session-1",
            "update": {
                "session_update": "tool_call",
                "tool_call_id": id,
                "status": "completed",
                "title": "rg",
                "kind": "search",
                "raw_input": { "pattern": "needle", "path": "." },
                "content": [{
                    "type": "content",
                    "content": { "type": "text", "text": output }
                }]
            }
        })
        .to_string(),
        position: Some("left".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now_ms() + offset_ms,
    }
}

// ── T6: Clone conversation ─────────────────────────────────────────

#[tokio::test]
async fn t6_2_clone_without_source() {
    let (svc, _repo, _b) = setup().await;

    let req: CloneConversationRequest = serde_json::from_value(json!({
        "conversation": {
            "type": "acp",
            "name": "Direct",
            "extra": { "agent_id": TEST_ACP_AGENT_ID }
        }
    }))
    .unwrap();
    let resp = svc.clone_create(USER_ID, req).await.unwrap();
    assert_eq!(resp.name, "Direct");
    // No source to merge from — only the caller-provided CreateConversationRequest
    // drives `extra`, so source-only keys (e.g. `contextFileName`) must not appear.
    assert!(resp.extra.get("contextFileName").is_none());
}

// ── T7: Reset conversation ─────────────────────────────────────────

#[tokio::test]
async fn t7_1_reset_clears_messages_and_status() {
    let (svc, repo, _b) = setup().await;

    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    // Insert messages
    for i in 0..3 {
        repo.insert_message(&make_message(conv.conversation_id.clone(), &format!("msg {i}"), i))
            .await
            .unwrap();
    }

    svc.reset(USER_ID, &conv.conversation_id.to_string()).await.unwrap();

    let fetched = svc.get(USER_ID, &conv.conversation_id.to_string()).await.unwrap();
    assert_eq!(fetched.status, ConversationStatus::Pending);

    let messages = svc
        .list_messages(USER_ID, &conv.conversation_id.to_string(), ListMessagesQuery::default())
        .await
        .unwrap();
    assert!(messages.items.is_empty());
    assert_eq!(messages.total, 0);
}

#[tokio::test]
async fn t7_3_reset_not_found() {
    let (svc, _repo, _b) = setup().await;
    let missing = nomifun_common::ConversationId::new();
    let err = svc.reset(USER_ID, missing.as_str()).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

// ── T8: Message list ───────────────────────────────────────────────

#[tokio::test]
async fn t8_1_empty_messages() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let result = svc
        .list_messages(USER_ID, &conv.conversation_id.to_string(), ListMessagesQuery::default())
        .await
        .unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
}

#[tokio::test]
async fn t8_2_pagination() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..10 {
        repo.insert_message(&make_message(conv.conversation_id.clone(), &format!("msg {i}"), i * 100))
            .await
            .unwrap();
    }

    let query = ListMessagesQuery {
        page: Some(1),
        page_size: Some(3),
        order: None,
        content_mode: None,
        cursor: None,
    };
    let result = svc.list_messages(USER_ID, &conv.conversation_id.to_string(), query).await.unwrap();
    assert_eq!(result.items.len(), 3);
    assert_eq!(result.total, 10);
    assert!(result.has_more);
}

#[tokio::test]
async fn t8_3_asc_order_default() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..3 {
        repo.insert_message(&make_message(conv.conversation_id.clone(), &format!("msg {i}"), i * 1000))
            .await
            .unwrap();
    }

    let result = svc
        .list_messages(USER_ID, &conv.conversation_id.to_string(), ListMessagesQuery::default())
        .await
        .unwrap();
    // ASC (default): oldest first
    assert!(result.items[0].created_at <= result.items[1].created_at);
    assert!(result.items[1].created_at <= result.items[2].created_at);
}

#[tokio::test]
async fn t8_4_asc_order() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..3 {
        repo.insert_message(&make_message(conv.conversation_id.clone(), &format!("msg {i}"), i * 1000))
            .await
            .unwrap();
    }

    let query = ListMessagesQuery {
        order: Some("ASC".into()),
        ..Default::default()
    };
    let result = svc.list_messages(USER_ID, &conv.conversation_id.to_string(), query).await.unwrap();
    assert!(result.items[0].created_at <= result.items[1].created_at);
    assert!(result.items[1].created_at <= result.items[2].created_at);
}

#[tokio::test]
async fn t8_5_conversation_not_found() {
    let (svc, _repo, _b) = setup().await;
    let missing = nomifun_common::ConversationId::new();
    let err = svc
        .list_messages(USER_ID, missing.as_str(), ListMessagesQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

// ── T9: Message search ─────────────────────────────────────────────

#[tokio::test]
async fn t8_6_compact_mode_truncates_large_tool_content_only_for_list_response() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    let large_output = "match line\n".repeat(10_000);

    let message_id = nomifun_common::MessageId::new();
    repo.insert_message(&make_acp_tool_message(
        conv.conversation_id.clone(),
        message_id.as_str(),
        &large_output,
        0,
    ))
        .await
        .unwrap();

    let full = svc
        .list_messages(USER_ID, &conv.conversation_id.to_string(), ListMessagesQuery::default())
        .await
        .unwrap();
    assert_eq!(
        full.items[0].content["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap(),
        large_output
    );

    let compact = svc
        .list_messages(
            USER_ID,
            &conv.conversation_id.to_string(),
            ListMessagesQuery {
                content_mode: Some("compact".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let compact_content = &compact.items[0].content;
    let preview = compact_content["update"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();

    assert!(compact_content["_compact"]["truncated"].as_bool().unwrap());
    assert!(compact_content["_compact"]["original_size"].as_u64().unwrap() > preview.len() as u64);
    assert!(preview.len() < large_output.len());
    assert!(!preview.contains(&large_output));
}

#[tokio::test]
async fn t8_7_get_message_returns_full_tool_content_after_compact_list() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    let large_output = "wide rg output\n".repeat(10_000);

    let message_id = nomifun_common::MessageId::new();
    repo.insert_message(&make_acp_tool_message(
        conv.conversation_id.clone(),
        message_id.as_str(),
        &large_output,
        0,
    ))
        .await
        .unwrap();

    let _ = svc
        .list_messages(
            USER_ID,
            &conv.conversation_id.to_string(),
            ListMessagesQuery {
                content_mode: Some("compact".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let detail = svc
        .get_message(USER_ID, &conv.conversation_id.to_string(), message_id.as_str())
        .await
        .unwrap();

    assert_eq!(
        detail.content["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap(),
        large_output
    );
}

#[tokio::test]
async fn t9_1_keyword_match() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    repo.insert_message(&make_message(conv.conversation_id.clone(), "Rust review report", 0))
        .await
        .unwrap();
    repo.insert_message(&make_message(conv.conversation_id.clone(), "Python test", 100))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "review".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.total, 1);

    let item = &result.items[0];
    assert_eq!(item.message_type, "text");
    assert!(item.message_created_at > 0);
    assert!(item.preview_text.contains("Rust review report"));

    assert_eq!(item.conversation.conversation_id, conv.conversation_id);
    assert_eq!(item.conversation.name, conv.name);
    assert_eq!(item.conversation.extra["workspace"], "/home/user/project");
}

#[tokio::test]
async fn t9_2_no_match() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(&make_message(conv.conversation_id.clone(), "hello world", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "xxxxnotexist".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
}

#[tokio::test]
async fn t9_3_search_pagination() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    for i in 0..5 {
        repo.insert_message(&make_message(conv.conversation_id.clone(), &format!("match keyword item {i}"), i * 100))
            .await
            .unwrap();
    }

    let query = SearchMessagesQuery {
        keyword: "keyword".into(),
        page: Some(1),
        page_size: Some(2),
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.total, 5);
    assert!(result.has_more);
}

#[tokio::test]
async fn t9_4_empty_keyword() {
    let (svc, _repo, _b) = setup().await;

    let query = SearchMessagesQuery {
        keyword: "".into(),
        page: None,
        page_size: None,
    };
    let err = svc.search_messages(USER_ID, query).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::BadRequest(_)));
}

#[tokio::test]
async fn t9_5_preview_text_extracts_from_json_content() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let complex_msg = MessageRow {
        id: 0,
        message_id: nomifun_common::MessageId::new().into_string(),
        conversation_id: conv.conversation_id.clone(),
        msg_id: None,
        r#type: "text".to_string(),
        content: r#"[{"type":"text","content":"Design document for search"},{"type":"text","content":"feature implementation"}]"#.to_string(),
        position: Some("right".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now_ms(),
    };
    repo.insert_message(&complex_msg).await.unwrap();

    let query = SearchMessagesQuery {
        keyword: "search".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 1);

    let item = &result.items[0];
    assert!(!item.preview_text.contains('{'));
    assert!(!item.preview_text.contains('['));
    assert!(item.preview_text.contains("Design document for search"));
    assert!(item.preview_text.contains("feature implementation"));
}

#[tokio::test]
async fn t9_6_search_result_includes_conversation_model() {
    let (svc, repo, _b) = setup().await;

    // Search surfaces conversation.model only for nomi (the only type that
    // carries a top-level model under the nomi-only rule).
    let nomi_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": "0190f5fe-7c00-7a00-8000-000000000001", "model": "claude-sonnet-4-20250514" },
        "extra": { "workspace": "/home/user/project" }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, nomi_req).await.unwrap();

    repo.insert_message(&make_message(conv.conversation_id.clone(), "model test keyword", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "model test".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert_eq!(result.items.len(), 1);

    let item = &result.items[0];
    let model = item.conversation.model.as_ref().unwrap();
    assert_eq!(model.provider_id, "0190f5fe-7c00-7a00-8000-000000000001");
    assert_eq!(model.model, "claude-sonnet-4-20250514");
}

#[tokio::test]
async fn t9_7_search_does_not_leak_other_users_messages() {
    let (svc, repo, _b) = setup().await;

    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(&make_message(conv.conversation_id.clone(), "secret keyword data", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "secret".into(),
        page: None,
        page_size: None,
    };
    let result = svc.search_messages("other_user_id", query).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
}

// ── T10: Associated conversations ──────────────────────────────────

#[tokio::test]
async fn t10_1_same_workspace() {
    let (svc, _repo, _b) = setup().await;

    let req1: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Conv A",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/shared/path" }
    }))
    .unwrap();
    let conv1 = svc.create(USER_ID, req1).await.unwrap();

    let req2: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Conv B",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/shared/path" }
    }))
    .unwrap();
    let conv2 = svc.create(USER_ID, req2).await.unwrap();

    // Different workspace
    let req3: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Conv C",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/other/path" }
    }))
    .unwrap();
    svc.create(USER_ID, req3).await.unwrap();

    let associated = svc.list_associated(USER_ID, &conv1.conversation_id.to_string()).await.unwrap();
    assert_eq!(associated.len(), 1);
    assert_eq!(associated[0].conversation_id, conv2.conversation_id);
}

#[tokio::test]
async fn t10_2_no_associated() {
    let (svc, _repo, _b) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "agent_id": TEST_ACP_AGENT_ID, "workspace": "/unique/path" }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, req).await.unwrap();

    let associated = svc.list_associated(USER_ID, &conv.conversation_id.to_string()).await.unwrap();
    assert!(associated.is_empty());
}

#[tokio::test]
async fn t10_3_associated_not_found() {
    let (svc, _repo, _b) = setup().await;
    let missing = nomifun_common::ConversationId::new();
    let err = svc.list_associated(USER_ID, missing.as_str()).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

// ── T12: Boundary scenarios ────────────────────────────────────────

#[tokio::test]
async fn t12_4_search_sql_injection() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(&make_message(conv.conversation_id.clone(), "safe content", 0))
        .await
        .unwrap();

    let query = SearchMessagesQuery {
        keyword: "'; DROP TABLE messages; --".into(),
        page: None,
        page_size: None,
    };
    // Should return empty results, not crash
    let result = svc.search_messages(USER_ID, query).await.unwrap();
    assert!(result.items.is_empty());
}

// ── Ownership cross-cutting ────────────────────────────────────────

#[tokio::test]
async fn messages_wrong_user_returns_not_found() {
    let (svc, repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    repo.insert_message(&make_message(conv.conversation_id.clone(), "hello", 0)).await.unwrap();

    let other_user = nomifun_common::UserId::new();
    let err = svc
        .list_messages(other_user.as_str(), &conv.conversation_id.to_string(), ListMessagesQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

#[tokio::test]
async fn reset_wrong_user_returns_not_found() {
    let (svc, _repo, _b) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let other_user = nomifun_common::UserId::new();
    let err = svc.reset(other_user.as_str(), &conv.conversation_id.to_string()).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}
