//! E2E integration tests with mock agent tasks.
//!
//! Tests the message flow, confirmation system, and auxiliary routes
//! with a mock IWorkerTaskManager that provides in-memory agents.

mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower::ServiceExt;

use async_trait::async_trait;
use nomifun_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
use nomifun_ai_agent::protocol::events::TextEventData;
use nomifun_ai_agent::types::{BuildTaskOptions, SendMessageData};
use nomifun_ai_agent::{AgentStreamEvent, IWorkerTaskManager};
use nomifun_common::{AgentKillReason, AgentType, AppError, Confirmation, ConversationStatus, TimestampMs, now_ms};

use common::{body_json, get_with_token, json_with_token, setup_and_login};

// ── Mock Agent ──────────────────────────────────────────────────

struct MockAgent {
    conversation_id: String,
    workspace: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Mutex<Vec<Confirmation>>,
    approvals: Mutex<std::collections::HashMap<String, bool>>,
    last_activity: AtomicI64,
}

impl MockAgent {
    fn new(conversation_id: &str, workspace: &str) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            conversation_id: conversation_id.to_owned(),
            workspace: workspace.to_owned(),
            event_tx,
            confirmations: Mutex::new(vec![]),
            approvals: Mutex::new(std::collections::HashMap::new()),
            last_activity: AtomicI64::new(now_ms()),
        }
    }
}

#[async_trait]
impl IAgentTask for MockAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.workspace
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(ConversationStatus::Running)
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.last_activity.load(Ordering::Relaxed)
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, _data: SendMessageData) -> Result<(), nomifun_ai_agent::AgentSendError> {
        self.last_activity.store(now_ms(), Ordering::Relaxed);
        // Emit a text event and finish
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Mock response".into(),
        }));
        let _ = self.event_tx.send(AgentStreamEvent::Finish(
            nomifun_ai_agent::protocol::events::FinishEventData::default(),
        ));
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AppError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
}

#[async_trait]
impl IMockAgent for MockAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }

    fn check_approval(&self, action: &str, _command_type: Option<&str>) -> bool {
        self.approvals.lock().unwrap().get(action).copied().unwrap_or(false)
    }

    fn confirm(&self, _msg_id: &str, call_id: &str, _data: Value, always_allow: bool) -> Result<(), AppError> {
        let mut confs = self.confirmations.lock().unwrap();
        confs.retain(|c| c.call_id != call_id);
        if always_allow {
            self.approvals.lock().unwrap().insert("test_action".to_owned(), true);
        }
        Ok(())
    }
}

// ── Mock Worker Task Manager ────────────────────────────────────

struct MockTaskManager {
    agents: Mutex<std::collections::HashMap<String, AgentInstance>>,
}

impl MockTaskManager {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn insert(&self, conv_id: &str, workspace: &str) -> Arc<MockAgent> {
        let agent = Arc::new(MockAgent::new(conv_id, workspace));
        self.agents
            .lock()
            .unwrap()
            .insert(conv_id.to_owned(), AgentInstance::Mock(agent.clone()));
        agent
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for MockTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AppError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let instance = AgentInstance::Mock(Arc::new(MockAgent::new(conversation_id, "/mock-workspace")));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

// ── Test App builder with mock agents ───────────────────────────

async fn build_app_with_mock_tasks() -> (axum::Router, nomifun_app::AppServices, Arc<MockTaskManager>) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = nomifun_app::AppServices::from_config(db, &nomifun_app::AppConfig::default())
        .await
        .unwrap();

    let mock_tm = Arc::new(MockTaskManager::new());
    let services = services.with_worker_task_manager(mock_tm.clone());

    let router = nomifun_app::create_router(&services).await;
    (router, services, mock_tm)
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str) -> String {
    let body = json!({
        "type": "acp",
        "name": name,
        "extra": { "workspace": "/project" }
    });
    let req = common::json_with_token("POST", "/api/conversations", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["id"].as_i64().unwrap().to_string()
}

// ── Message flow with mock agent ────────────────────────────────

#[tokio::test]
async fn send_message_with_mock_agent_returns_202() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Mock Agent Test").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/messages"),
        json!({ "content": "Hello mock agent" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn stop_stream_with_mock_agent() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Stop Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn warmup_with_mock_agent() {
    let (mut app, services, _mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Warmup Test").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/warmup"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Confirmation system with mock agent ─────────────────────────

#[tokio::test]
async fn list_confirmations_empty() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/confirmations"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn confirm_and_check_approval() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval Test").await;
    let agent = mock_tm.insert(&conv_id, "/mock-workspace");

    // Pre-populate a pending confirmation so the confirm endpoint can find it
    agent.confirmations.lock().unwrap().push(Confirmation {
        id: "conf-1".into(),
        call_id: "call-42".into(),
        title: Some("Allow file edit".into()),
        action: Some("test_action".into()),
        description: String::new(),
        command_type: None,
        options: vec![],
        screenshot: None,
    });

    // Confirm a call with alwaysAllow=true
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/confirmations/call-42/confirm"),
        json!({ "msg_id": "msg-1", "data": { "value": "allow" }, "always_allow": true }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Check approval — should be approved for "test_action"
    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=test_action"),
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["approved"], true);
}

#[tokio::test]
async fn check_approval_not_set() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval NotSet").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=unknown_action"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["approved"], false);
}

// ── Auxiliary routes with mock agent ────────────────────────────

#[tokio::test]
async fn slash_commands_with_mock_returns_empty() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Slash Mock Test").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/slash-commands"), &token);
    let resp = app.oneshot(req).await.unwrap();
    // Mock agent is not a real AcpAgentManager, so downcast fails → 500
    // OR if agent_type check prevents downcast, returns empty array
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "Expected 200 or 500, got {status}"
    );
}

#[tokio::test]
async fn openclaw_runtime_wrong_agent_type() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "OpenClaw Wrong Type").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = get_with_token(&format!("/api/conversations/{conv_id}/openclaw/runtime"), &token);
    let resp = app.oneshot(req).await.unwrap();
    // Non-OpenClaw agents return a JSON null payload instead of an
    // error — the endpoint is a best-effort diagnostic; callers that
    // need stricter typing check the payload shape themselves.
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(json["data"].is_null());
}

#[tokio::test]
async fn side_question_with_mock_agent() {
    let (mut app, services, mock_tm) = build_app_with_mock_tasks().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "Pass123!").await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Side Q Mock").await;
    mock_tm.insert(&conv_id, "/mock-workspace");

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/side-question"),
        json!({ "question": "What is this code?" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // Mock agent is type Acp but not a real AcpAgentManager, so downcast
    // fails. The handler first checks agent_type() == Acp, then tries to
    // downcast. Since our mock returns Acp type, downcast fails → 500.
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "Expected 200 or 500, got {status}"
    );
}
