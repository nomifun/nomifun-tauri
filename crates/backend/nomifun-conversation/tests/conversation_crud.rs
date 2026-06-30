use std::sync::Arc;

use nomifun_ai_agent::IWorkerTaskManager;
use nomifun_api_types::{
    CreateConversationRequest, ListConversationsQuery, UpdateConversationRequest, WebSocketMessage,
};
use nomifun_common::{AgentKillReason, AgentType, AppError, ConversationSource, ConversationStatus, TimestampMs};
use nomifun_conversation::ConversationService;
use nomifun_conversation::skill_resolver::SkillResolver;
use nomifun_db::{SqliteConversationRepository, init_database_memory};
use nomifun_realtime::EventBroadcaster;
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

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        std::mem::take(&mut self.events.lock().unwrap())
    }
}

impl EventBroadcaster for TestBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct NoopTaskManager;

#[async_trait::async_trait]
impl IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<nomifun_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: nomifun_ai_agent::types::BuildTaskOptions,
    ) -> Result<nomifun_ai_agent::AgentInstance, AppError> {
        Err(AppError::Internal("noop".into()))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
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

async fn setup() -> (ConversationService, Arc<TestBroadcaster>, Arc<dyn IWorkerTaskManager>) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
        Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
        Arc::new(nomifun_db::SqliteAcpSessionRepository::new(db.pool().clone()));
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(EmptySkillResolver),
        task_mgr.clone(),
        repo,
        agent_metadata_repo,
        acp_session_repo,
    );
    (svc, broadcaster, task_mgr)
}

const USER_ID: &str = "system_default_user";

fn make_create_req() -> CreateConversationRequest {
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/home/user/project" }
    }))
    .unwrap()
}

// ── T1: Create conversation ────────────────────────────────────────

#[tokio::test]
async fn t1_1_create_with_defaults() {
    let (svc, broadcaster, _task_mgr) = setup().await;

    let resp = svc.create(USER_ID, make_create_req()).await.unwrap();

    assert!(resp.id > 0);
    assert_eq!(resp.r#type, AgentType::Acp);
    assert_eq!(resp.status, ConversationStatus::Pending);
    assert_eq!(resp.source, Some(ConversationSource::Nomifun));
    assert!(!resp.pinned);
    assert!(resp.pinned_at.is_none());
    assert_eq!(resp.extra["workspace"], "/home/user/project");
    assert!(resp.created_at > 0);
    assert_eq!(resp.created_at, resp.modified_at);

    // Non-nomi: top-level model is None.
    assert!(resp.model.is_none(), "ACP response should not carry top-level model");

    // WebSocket event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "conversation.listChanged");
    assert_eq!(events[0].data["action"], "created");
    assert_eq!(events[0].data["conversation_id"], resp.id);
    assert_eq!(events[0].data["source"], "nomifun");
}

#[tokio::test]
async fn t1_2_create_each_agent_type() {
    let (svc, _, _task_mgr) = setup().await;

    let types = vec![
        ("acp", AgentType::Acp),
        ("openclaw-gateway", AgentType::OpenclawGateway),
        ("nanobot", AgentType::Nanobot),
        ("remote", AgentType::Remote),
        ("nomi", AgentType::Nomi),
    ];

    for (type_str, expected_type) in types {
        let body = if type_str == "nomi" {
            json!({
                "type": type_str,
                "model": { "provider_id": "p1", "model": "m1" },
                "extra": {}
            })
        } else {
            json!({
                "type": type_str,
                "extra": {}
            })
        };
        let req: CreateConversationRequest = serde_json::from_value(body).unwrap();
        let resp = svc.create(USER_ID, req).await.unwrap();
        assert_eq!(resp.r#type, expected_type, "Type mismatch for {type_str}");
        if type_str == "nomi" {
            assert!(resp.model.is_some(), "nomi should keep top-level model");
        } else {
            assert!(resp.model.is_none(), "{type_str} should have no top-level model");
        }
    }
}

#[tokio::test]
async fn t1_3_create_with_optional_fields() {
    let (svc, _, _task_mgr) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Custom Name",
        "source": "telegram",
        "channel_chat_id": "user:123",
        "extra": { "workspace": "/path" }
    }))
    .unwrap();
    let resp = svc.create(USER_ID, req).await.unwrap();

    assert_eq!(resp.name, "Custom Name");
    assert_eq!(resp.source, Some(ConversationSource::Telegram));
    assert_eq!(resp.channel_chat_id.as_deref(), Some("user:123"));
}

// ── T2: List conversations ─────────────────────────────────────────

#[tokio::test]
async fn t2_1_list_empty() {
    let (svc, _, _task_mgr) = setup().await;
    let result = svc.list(USER_ID, ListConversationsQuery::default(), false).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
    assert!(!result.has_more);
}

#[tokio::test]
async fn t2_2_list_basic() {
    let (svc, _, _task_mgr) = setup().await;
    for _ in 0..3 {
        svc.create(USER_ID, make_create_req()).await.unwrap();
    }

    let result = svc.list(USER_ID, ListConversationsQuery::default(), false).await.unwrap();
    assert_eq!(result.items.len(), 3);
    assert_eq!(result.total, 3);
}

#[tokio::test]
async fn t2_3_cursor_pagination() {
    let (svc, _, _task_mgr) = setup().await;
    for _ in 0..5 {
        svc.create(USER_ID, make_create_req()).await.unwrap();
    }

    // First page: limit=2
    let query = ListConversationsQuery {
        limit: Some(2),
        ..Default::default()
    };
    let page1 = svc.list(USER_ID, query, false).await.unwrap();
    assert_eq!(page1.items.len(), 2);
    assert!(page1.has_more);
    assert_eq!(page1.total, 5);

    // Second page: cursor = last ID from page 1
    let cursor = page1.items.last().unwrap().id;
    let query2 = ListConversationsQuery {
        cursor: Some(cursor.to_string()),
        limit: Some(2),
        ..Default::default()
    };
    let page2 = svc.list(USER_ID, query2, false).await.unwrap();
    assert_eq!(page2.items.len(), 2);
    assert!(page2.has_more);

    // Third page
    let cursor2 = page2.items.last().unwrap().id;
    let query3 = ListConversationsQuery {
        cursor: Some(cursor2.to_string()),
        limit: Some(2),
        ..Default::default()
    };
    let page3 = svc.list(USER_ID, query3, false).await.unwrap();
    assert_eq!(page3.items.len(), 1);
    assert!(!page3.has_more);

    // No overlap between pages
    let all_ids: Vec<i64> = page1
        .items
        .iter()
        .chain(page2.items.iter())
        .chain(page3.items.iter())
        .map(|c| c.id)
        .collect();
    let unique: std::collections::HashSet<&i64> = all_ids.iter().collect();
    assert_eq!(all_ids.len(), unique.len());
}

#[tokio::test]
async fn t2_4_source_filter() {
    let (svc, _, _task_mgr) = setup().await;

    // 2 nomifun + 1 telegram
    svc.create(USER_ID, make_create_req()).await.unwrap();
    svc.create(USER_ID, make_create_req()).await.unwrap();

    let telegram_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "source": "telegram",
        "extra": {}
    }))
    .unwrap();
    svc.create(USER_ID, telegram_req).await.unwrap();

    let query = ListConversationsQuery {
        source: Some("telegram".into()),
        ..Default::default()
    };
    let result = svc.list(USER_ID, query, false).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].source, Some(ConversationSource::Telegram));
}

#[tokio::test]
async fn t2_5_pinned_filter() {
    let (svc, _, task_mgr) = setup().await;

    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    svc.create(USER_ID, make_create_req()).await.unwrap();

    // Pin one
    let pin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    svc.update(USER_ID, &conv.id.to_string(), pin_req, &task_mgr).await.unwrap();

    let query = ListConversationsQuery {
        pinned: Some(true),
        ..Default::default()
    };
    let result = svc.list(USER_ID, query, false).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert!(result.items[0].pinned);
}

// ── T3: Get single conversation ────────────────────────────────────

#[tokio::test]
async fn t3_1_get_existing() {
    let (svc, _, _task_mgr) = setup().await;
    let created = svc.create(USER_ID, make_create_req()).await.unwrap();

    let fetched = svc.get(USER_ID, &created.id.to_string()).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.r#type, created.r#type);
    assert_eq!(fetched.name, created.name);
    assert_eq!(fetched.status, created.status);
}

#[tokio::test]
async fn t3_2_get_not_found() {
    let (svc, _, _task_mgr) = setup().await;
    let err = svc.get(USER_ID, "non-existent-uuid").await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

// ── T4: Update conversation ────────────────────────────────────────

#[tokio::test]
async fn t4_1_update_name() {
    let (svc, broadcaster, task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "New Name" })).unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();

    assert_eq!(updated.name, "New Name");
    assert!(updated.modified_at >= conv.modified_at);

    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "updated");
}

#[tokio::test]
async fn t4_2_pin_conversation() {
    let (svc, _, task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();

    assert!(updated.pinned);
    assert!(updated.pinned_at.is_some());
}

#[tokio::test]
async fn t4_3_unpin_clears_pinned_at() {
    let (svc, _, task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    // Pin
    let pin: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let pinned = svc.update(USER_ID, &conv.id.to_string(), pin, &task_mgr).await.unwrap();
    assert!(pinned.pinned_at.is_some());

    // Unpin
    let unpin: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": false })).unwrap();
    let unpinned = svc.update(USER_ID, &conv.id.to_string(), unpin, &task_mgr).await.unwrap();
    assert!(!unpinned.pinned);
    assert!(unpinned.pinned_at.is_none());
}

#[tokio::test]
async fn t4_4_extra_merge_preserves_existing_keys() {
    let (svc, _, task_mgr) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/old", "contextFileName": "ctx.md" }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, req).await.unwrap();

    // Update only workspace
    let update_req: UpdateConversationRequest =
        serde_json::from_value(json!({ "extra": { "workspace": "/new" } })).unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), update_req, &task_mgr).await.unwrap();

    assert_eq!(updated.extra["workspace"], "/new");
    assert_eq!(updated.extra["contextFileName"], "ctx.md");
}

#[tokio::test]
async fn t4_5_update_model() {
    let (svc, _, task_mgr) = setup().await;

    // Top-level model updates are only valid on nomi conversations
    // (Task 8 enforces the nomi-only rule in update).
    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": "p1", "model": "m1" },
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create(USER_ID, create_req).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "model": { "provider_id": "p2", "model": "new-model" }
    }))
    .unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();

    let model = updated.model.unwrap();
    assert_eq!(model.provider_id, "p2");
    assert_eq!(model.model, "new-model");
}

#[tokio::test]
async fn t4_6_update_not_found() {
    let (svc, _, task_mgr) = setup().await;
    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "x" })).unwrap();
    let err = svc.update(USER_ID, "non-existent", req, &task_mgr).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

// ── T5: Delete conversation ────────────────────────────────────────

#[tokio::test]
async fn t5_1_delete_conversation() {
    let (svc, broadcaster, _task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    broadcaster.take_events();

    svc.delete(USER_ID, &conv.id.to_string()).await.unwrap();

    // Verify gone
    let err = svc.get(USER_ID, &conv.id.to_string()).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));

    // Verify broadcast
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "deleted");
    assert_eq!(events[0].data["conversation_id"], conv.id);
}

#[tokio::test]
async fn t5_2_delete_then_get_returns_404() {
    let (svc, _, _task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    svc.delete(USER_ID, &conv.id.to_string()).await.unwrap();
    let err = svc.get(USER_ID, &conv.id.to_string()).await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

#[tokio::test]
async fn t5_3_delete_not_found() {
    let (svc, _, _task_mgr) = setup().await;
    let err = svc.delete(USER_ID, "non-existent").await.unwrap_err();
    assert!(matches!(err, nomifun_common::AppError::NotFound(_)));
}

// ── T11: WebSocket event verification ──────────────────────────────

#[tokio::test]
async fn t11_1_create_broadcasts_created() {
    let (svc, broadcaster, _task_mgr) = setup().await;
    let resp = svc.create(USER_ID, make_create_req()).await.unwrap();

    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "conversation.listChanged");
    assert_eq!(events[0].data["action"], "created");
    assert_eq!(events[0].data["conversation_id"], resp.id);
}

#[tokio::test]
async fn t11_2_update_broadcasts_updated() {
    let (svc, broadcaster, task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    broadcaster.take_events();

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "x" })).unwrap();
    svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();

    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "updated");
}

#[tokio::test]
async fn t11_3_delete_broadcasts_deleted() {
    let (svc, broadcaster, _task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();
    broadcaster.take_events();

    svc.delete(USER_ID, &conv.id.to_string()).await.unwrap();

    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "deleted");
}

// ── T12: Boundary scenarios ────────────────────────────────────────

#[tokio::test]
async fn t12_1_long_name() {
    let (svc, _, _task_mgr) = setup().await;
    let long_name = "x".repeat(1000);

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": long_name,
        "extra": {}
    }))
    .unwrap();
    let resp = svc.create(USER_ID, req).await.unwrap();
    assert_eq!(resp.name.len(), 1000);
}

#[tokio::test]
async fn t12_2_large_extra_json() {
    let (svc, _, _task_mgr) = setup().await;

    let large_extra = json!({
        "workspace": "/project",
        "nested": {
            "deep": {
                "array": [1, 2, 3, 4, 5],
                "object": { "key": "value" }
            }
        },
        "list": (0..100).collect::<Vec<_>>()
    });

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": large_extra
    }))
    .unwrap();
    let resp = svc.create(USER_ID, req).await.unwrap();

    assert_eq!(resp.extra["workspace"], "/project");
    assert_eq!(resp.extra["nested"]["deep"]["array"][2], 3);
}

#[tokio::test]
async fn t12_3_concurrent_creates() {
    let (svc, _, _task_mgr) = setup().await;

    let mut handles = vec![];
    for _ in 0..10 {
        let svc = svc.clone();
        handles.push(tokio::spawn(async move {
            svc.create(USER_ID, make_create_req()).await.unwrap()
        }));
    }

    let mut ids = vec![];
    for handle in handles {
        let resp = handle.await.unwrap();
        ids.push(resp.id);
    }

    // All IDs unique
    let unique: std::collections::HashSet<&i64> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len());
}

// ── Full lifecycle ─────────────────────────────────────────────────

#[tokio::test]
async fn full_lifecycle_create_get_update_delete() {
    let (svc, broadcaster, task_mgr) = setup().await;

    // Create
    let created = svc.create(USER_ID, make_create_req()).await.unwrap();
    assert_eq!(created.status, ConversationStatus::Pending);

    // Get
    let fetched = svc.get(USER_ID, &created.id.to_string()).await.unwrap();
    assert_eq!(fetched.id, created.id);

    // Update
    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "name": "Updated",
        "pinned": true,
        "extra": { "workspace": "/updated" }
    }))
    .unwrap();
    let updated = svc.update(USER_ID, &created.id.to_string(), update_req, &task_mgr).await.unwrap();
    assert_eq!(updated.name, "Updated");
    assert!(updated.pinned);
    assert_eq!(updated.extra["workspace"], "/updated");

    // Delete
    svc.delete(USER_ID, &created.id.to_string()).await.unwrap();
    assert!(svc.get(USER_ID, &created.id.to_string()).await.is_err());

    // Verify all events: created + updated + deleted
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].data["action"], "created");
    assert_eq!(events[1].data["action"], "updated");
    assert_eq!(events[2].data["action"], "deleted");
}

// ── Type-aware model rules ─────────────────────────────────────────

#[tokio::test]
async fn create_rejects_top_level_model_for_acp() {
    let (svc, _, _task_mgr) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "model": { "provider_id": "p1", "model": "claude-sonnet-4-20250514" },
        "extra": {}
    }))
    .unwrap();

    let err = svc.create(USER_ID, req).await.unwrap_err();
    match err {
        AppError::BadRequest(msg) => {
            assert!(msg.contains("model"), "error message should mention model: {msg}");
            assert!(msg.contains("extra"), "error message should mention extra: {msg}");
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn create_rejects_top_level_model_for_remote() {
    let (svc, _, _task_mgr) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "remote",
        "model": { "provider_id": "p1", "model": "m1" },
        "extra": {}
    }))
    .unwrap();

    assert!(matches!(svc.create(USER_ID, req).await, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn create_accepts_top_level_model_for_nomi() {
    let (svc, _, _task_mgr) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": "p1", "model": "gpt-4o" },
        "extra": {}
    }))
    .unwrap();

    let resp = svc.create(USER_ID, req).await.unwrap();
    assert_eq!(resp.r#type, AgentType::Nomi);
    let model = resp.model.expect("nomi response should carry top-level model");
    assert_eq!(model.provider_id, "p1");
    assert_eq!(model.model, "gpt-4o");
}

#[tokio::test]
async fn create_nomi_strips_extra_model_field() {
    let (svc, _, _task_mgr) = setup().await;

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": "p1", "model": "gpt-4o" },
        "extra": {
            "workspace": "/home/user/project",
            "model": "bogus-from-legacy-client"
        }
    }))
    .unwrap();

    let resp = svc.create(USER_ID, req).await.unwrap();
    assert!(
        !resp.extra.as_object().unwrap().contains_key("model"),
        "nomi create must strip extra.model to avoid dual source of truth; got {:?}",
        resp.extra
    );
    // Top-level model is still present and wins.
    assert_eq!(resp.model.unwrap().model, "gpt-4o");
}

#[tokio::test]
async fn update_rejects_top_level_model_for_acp() {
    let (svc, _, task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "model": { "provider_id": "p1", "model": "claude-sonnet-4-20250514" }
    }))
    .unwrap();

    let err = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap_err();
    assert!(
        matches!(err, AppError::BadRequest(_)),
        "expected BadRequest, got {err:?}"
    );
}

#[tokio::test]
async fn update_accepts_top_level_model_for_nomi() {
    let (svc, _, task_mgr) = setup().await;

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": "p1", "model": "gpt-4o" },
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create(USER_ID, create_req).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "model": { "provider_id": "p1", "model": "gpt-4o-mini" }
    }))
    .unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();
    assert_eq!(updated.model.unwrap().model, "gpt-4o-mini");
}

#[tokio::test]
async fn update_non_nomi_extra_model_does_not_kill_task() {
    // Verifies the explicit rule that `extra.model` changes for non-nomi
    // do NOT trigger task_manager.kill. Since our `NoopTaskManager::kill` is
    // a no-op we can't assert the negative directly; we assert the update
    // succeeds and the merged extra carries the new field, and that top-level
    // model remains None.
    let (svc, _, task_mgr) = setup().await;
    let conv = svc.create(USER_ID, make_create_req()).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "current_model_id": "claude-opus-4" }
    }))
    .unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();
    assert_eq!(updated.extra["current_model_id"], "claude-opus-4");
    assert!(updated.model.is_none());
}

#[tokio::test]
async fn update_nomi_strips_extra_model_from_patch() {
    let (svc, _, task_mgr) = setup().await;

    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "nomi",
        "model": { "provider_id": "p1", "model": "gpt-4o" },
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create(USER_ID, create_req).await.unwrap();

    // Client mistakenly sends extra.model on an nomi PATCH. It should be
    // silently stripped from the merged extra, not persisted.
    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "model": "legacy-value", "last_token_usage": { "total_tokens": 42 } }
    }))
    .unwrap();
    let updated = svc.update(USER_ID, &conv.id.to_string(), req, &task_mgr).await.unwrap();

    assert!(
        !updated.extra.as_object().unwrap().contains_key("model"),
        "nomi PATCH must strip extra.model; got {:?}",
        updated.extra
    );
    // Other extra keys from the patch are merged as usual.
    assert_eq!(updated.extra["last_token_usage"]["total_tokens"], 42);
    // Top-level model unchanged by the extra-only patch.
    assert_eq!(updated.model.unwrap().model, "gpt-4o");
}

#[tokio::test]
async fn create_acp_seeds_acp_session_runtime_from_extra() {
    use nomifun_db::{SqliteAcpSessionRepository, init_database_memory};

    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(nomifun_db::SqliteConversationRepository::new(db.pool().clone()));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
        Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
        Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let svc = nomifun_conversation::ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(EmptySkillResolver),
        task_mgr,
        repo,
        agent_metadata_repo,
        acp_session_repo.clone(),
    );

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "backend": "claude",
            "current_mode_id": "bypassPermissions",
            "current_model_id": "claude-opus-4"
        }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, req).await.unwrap();

    let runtime = acp_session_repo
        .load_runtime_state(conv.id)
        .await
        .unwrap()
        .expect("acp_session runtime state should exist after create");
    assert_eq!(
        runtime.current_mode_id.as_deref(),
        Some("bypassPermissions"),
        "extra.current_mode_id must be seeded into acp_session on create"
    );
    assert_eq!(
        runtime.current_model_id.as_deref(),
        Some("claude-opus-4"),
        "extra.current_model_id must be seeded into acp_session on create"
    );
}

#[tokio::test]
async fn create_acp_skips_seed_when_extra_has_empty_runtime_fields() {
    use nomifun_db::{SqliteAcpSessionRepository, init_database_memory};

    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(nomifun_db::SqliteConversationRepository::new(db.pool().clone()));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn nomifun_db::IAgentMetadataRepository> =
        Arc::new(nomifun_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn nomifun_db::IAcpSessionRepository> =
        Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let svc = nomifun_conversation::ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(EmptySkillResolver),
        task_mgr,
        repo,
        agent_metadata_repo,
        acp_session_repo.clone(),
    );

    // Both fields present but empty — treated as absent, no save_runtime_state call.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "backend": "claude", "current_mode_id": "", "current_model_id": "" }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, req).await.unwrap();

    let runtime = acp_session_repo.load_runtime_state(conv.id).await.unwrap();
    // Either `None` (no runtime key yet) or Some(default) — both mean "nothing seeded".
    assert!(
        runtime
            .as_ref()
            .is_none_or(|r| r.current_mode_id.is_none() && r.current_model_id.is_none()),
        "empty runtime fields should not produce a seed: got {runtime:?}"
    );
}

// ── link_orchestrator_run: merge extra.orchestrator_run_id + broadcast ──

#[tokio::test]
async fn link_orchestrator_run_merges_run_id_and_broadcasts() {
    let (svc, broadcaster, _task_mgr) = setup().await;

    // Seed a conversation with a pre-existing extra key (workspace) so we can
    // prove the link MERGES rather than replaces extra.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": "/home/user/project" }
    }))
    .unwrap();
    let conv = svc.create(USER_ID, req).await.unwrap();
    // Discard the create broadcast so we only observe the link event.
    broadcaster.take_events();

    svc.link_orchestrator_run(&conv.id.to_string(), "run_abc").await.unwrap();

    // Re-read the conversation: the run id is written and workspace survives.
    let fetched = svc.get(USER_ID, &conv.id.to_string()).await.unwrap();
    assert_eq!(
        fetched.extra["orchestrator_run_id"], "run_abc",
        "link must write extra.orchestrator_run_id"
    );
    assert_eq!(
        fetched.extra["workspace"], "/home/user/project",
        "link must merge (not replace) extra — workspace must survive"
    );

    // Exactly one conversation.listChanged(updated) for this conversation.
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1, "link must broadcast exactly one event");
    assert_eq!(events[0].name, "conversation.listChanged");
    assert_eq!(events[0].data["action"], "updated");
    assert_eq!(events[0].data["conversation_id"], conv.id);
}

#[tokio::test]
async fn link_orchestrator_run_empty_conversation_id_is_noop() {
    let (svc, broadcaster, _task_mgr) = setup().await;

    // Empty conversation_id (MCP / no-session callers) must be a silent no-op:
    // Ok(()) and zero broadcasts.
    let result = svc.link_orchestrator_run("", "run_x").await;
    assert!(result.is_ok(), "empty conversation_id must return Ok(()): {result:?}");

    let events = broadcaster.take_events();
    assert!(events.is_empty(), "empty conversation_id must broadcast nothing, got {events:?}");
}
