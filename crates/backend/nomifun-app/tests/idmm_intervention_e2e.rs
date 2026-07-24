//! End-to-end guard that IDMM ACTUALLY INTERVENES — not just that its config
//! round-trips (which `idmm_e2e.rs` already covers).
//!
//! REGRESSION CONTEXT: 智能决策「无法主动触发决策 / 完全不可用」recurred several times
//! (wrong-instance supervision hook — 6f7df38f; persisted gateway-as-routing —
//! 74d85a5c + b2777ddd, fixed by ef487298; the on-arm pending-confirmation gap)
//! WITHOUT a single failing test, because the only IDMM integration tests
//! covered config persistence and never the arm → detect → intervene path.
//!
//! This test reproduces the user's exact gesture — a plain desktop nomi
//! conversation whose agent is BLOCKED on a tool-permission "选择项" that was
//! emitted BEFORE 智能决策 was enabled — and asserts the decision watch recovers
//! that pending confirmation on arm and auto-confirms it. `observe()` only sees
//! FUTURE events (it missed the pre-arm permission), so the on-arm
//! `pending_signal` lane is the ONLY one that can recover it; before the fix it
//! scanned persisted chat TEXT only and never saw a structured confirmation, so
//! the agent stayed blocked forever and IDMM was silent. It fails if any link in
//! arm / on_turn_start / ensure / pending_signal / policy / inject breaks.

mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use nomifun_ai_agent::types::{AgentRuntimeBuildOptions, SendMessageData};
use nomifun_ai_agent::{
    AgentRuntimeHandle, AgentSendError, AgentStreamEvent, AgentRuntimeControl, MockAgentRuntime, AgentRuntimeRegistry, InMemoryAgentRuntimeRegistry,
};
use nomifun_app::{AppConfig, AppServices, create_router};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, Confirmation, ConfirmationOption, ConversationStatus,
    ProviderId, TimestampMs, now_ms,
};

use common::{body_json, json_with_token, setup_and_login};

/// A mock agent permanently BLOCKED on one safe (read-only) tool confirmation —
/// the structured "选择项" the agent emitted before the watch armed. Records
/// every `confirm()` it receives so the test can assert IDMM answered it.
struct BlockedOnConfirmationAgent {
    conversation_id: String,
    confirmed: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl AgentRuntimeControl for BlockedOnConfirmationAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Nomi
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        "/tmp/test"
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
    }
    fn is_transport_healthy(&self) -> bool {
        true
    }
    fn last_activity_at(&self) -> TimestampMs {
        now_ms()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentStreamEvent> {
        // No live sender → the observe() live lane sees a closed stream, so the
        // ONLY way IDMM can act is the on-arm pending_signal lane (the point).
        let (tx, _) = tokio::sync::broadcast::channel(1);
        tx.subscribe()
    }
    async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AppError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AppError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl MockAgentRuntime for BlockedOnConfirmationAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        vec![Confirmation {
            id: "conf_1".into(),
            call_id: "call_42".into(),
            title: Some("读取 package.json".into()),
            action: None,
            description: "允许读取该文件?".into(),
            // read-only → command_type_is_safe → the rule tier auto-confirms the
            // safe "proceed once" option without needing a backup model.
            command_type: Some("read".into()),
            options: vec![
                ConfirmationOption {
                    label: "允许一次".into(),
                    value: json!("proceed_once"),
                    params: None,
                },
                ConfirmationOption {
                    label: "拒绝".into(),
                    value: json!("cancel"),
                    params: None,
                },
            ],
            screenshot: None,
        }]
    }
    fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        _data: serde_json::Value,
        _always_allow: bool,
    ) -> Result<(), AppError> {
        self.confirmed.lock().unwrap().push(call_id.to_string());
        Ok(())
    }
}

/// Build an app whose agent factory returns a `BlockedOnConfirmationAgent`,
/// sharing a recorder so the test can observe IDMM's auto-confirm.
async fn build_app_blocked_on_confirmation() -> (axum::Router, AppServices, Arc<Mutex<Vec<String>>>) {
    let confirmed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_factory = confirmed.clone();
    let db = nomifun_db::init_database_memory().await.unwrap();
    let factory: Arc<
        dyn Fn(AgentRuntimeBuildOptions) -> futures_util::future::BoxFuture<'static, Result<AgentRuntimeHandle, AppError>>
            + Send
            + Sync,
    > = Arc::new(move |opts: AgentRuntimeBuildOptions| {
        let confirmed = confirmed_factory.clone();
        Box::pin(async move {
            Ok(AgentRuntimeHandle::Mock(Arc::new(BlockedOnConfirmationAgent {
                conversation_id: opts.conversation_id,
                confirmed,
            })))
        })
    });
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(InMemoryAgentRuntimeRegistry::new(factory));
    let services = AppServices::from_config(db, &AppConfig::default())
        .await
        .unwrap()
        .with_agent_runtime_registry(runtime_registry);
    let router = create_router(&services).await;
    (router, services, confirmed)
}

/// Nomi conversations require a canonical persisted provider/model binding
/// before the runtime factory seam is reached. Keep that production invariant
/// intact in this intervention-focused fixture.
async fn seed_mock_provider(services: &AppServices) -> String {
    let provider_id = ProviderId::new().into_string();
    nomifun_db::sqlx::query(
        "INSERT INTO providers \
         (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
          capabilities, created_at, updated_at) \
         VALUES (?, 'openai', 'IDMM intervention mock', 'https://example.invalid', \
                 'encrypted', '[\"mock-model\"]', 1, '[]', 1, 1)",
    )
    .bind(&provider_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    provider_id
}

#[tokio::test]
async fn idmm_recovers_and_confirms_on_arm_pending_tool_confirmation() {
    let (mut app, services, confirmed) = build_app_blocked_on_confirmation().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = seed_mock_provider(&services).await;

    // A plain desktop nomi conversation (no channel/companion markers, no
    // channel_chat_id → is_plain_desktop / not-routed → IDMM may auto-answer).
    let conv = {
        let body = json!({
            "type": "nomi",
            "name": "idmm-intervene",
            "model": {
                "provider_id": provider_id,
                "model": "mock-model",
                "use_model": null
            },
            "extra": { "workspace": "/project" }
        });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/conversations", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create conversation failed: {}", resp.status());
        body_json(resp).await["data"]["conversation_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // Enable 决策值守 at the rule tier — a safe read-only confirmation is
    // auto-confirmed by the rule tier (no backup model required).
    let body = json!({
        "kind": "conversation",
        "target_id": conv,
        "decision_watch": { "enabled": true, "tier": "rule_only" }
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/idmm", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "enabling the decision watch should succeed");

    // Send a message: this builds + registers the (already confirmation-blocked)
    // Agent runtime and fires on_turn_start, which arms IDMM. The supervisor's
    // pending_signal must recover the live pending confirmation and auto-confirm
    // it — the agent emitted no future events (closed stream), so the on-arm lane
    // is the only one that can act.
    let body = json!({ "content": "帮我写一个贪吃蛇游戏，并在每个设计环节都回复我" });
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/conversations/{conv}/messages"),
            body,
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "send_message should accept the turn, got {}",
        resp.status()
    );

    // Arming + pending_signal + inject happen on a detached task; poll for the
    // auto-confirm (no backoff on the on-arm pending decision, so it is prompt).
    let mut answered = false;
    for _ in 0..80 {
        if confirmed.lock().unwrap().iter().any(|c| c == "call_42") {
            answered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        answered,
        "IDMM must recover the on-arm pending tool-confirmation and auto-confirm call_42 (decision watch was inert on pending confirmations); confirmed={:?}",
        confirmed.lock().unwrap()
    );
}
