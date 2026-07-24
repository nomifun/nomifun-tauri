//! End-to-end guard that AutoWork + IDMM ENABLED TOGETHER actually cooperate —
//! the user's report: 「同时开启 自动工作 + 智能决策, 两个功能都无法正常配合工作 / 彻底不工作」.
//!
//! ROOT CAUSE (verified against the running dev DB + logs, conv 32/33): a tag a
//! prior failure left PAUSED is a GLOBAL per-tag state. Every conversation bound
//! to the same tag inherits the pause, so AutoWork never claims anything and the
//! session sits dead — with no per-conversation indication that the shared tag is
//! paused. The fix: an explicit AutoWork ENABLE auto-resumes a paused tag (and
//! gives its stuck requirements a fresh attempt budget), so toggling 自动工作 on
//! actually RUNS instead of silently inheriting the stale pause.
//!
//! This test reproduces the gesture: a requirement is driven to failure until its
//! tag pauses, THEN — with 智能决策 also enabled, exactly like the user — AutoWork
//! is enabled on a conversation bound to that tag. It asserts (1) the tag is
//! resumed by the enable (deterministic, synchronous in the handler) and (2) the
//! AutoWork loop then claims + runs the requirement to a processed terminal state
//! (`needs_review` for a Nomi session whose clean turn produced no verdict tool
//! call). It fails if enable does not auto-resume, or if the two features wedge
//! each other so the requirement never progresses.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use nomifun_ai_agent::protocol::events::{FinishEventData, TextEventData};
use nomifun_ai_agent::types::{AgentRuntimeBuildOptions, SendMessageData};
use nomifun_ai_agent::{
    AgentRuntimeHandle, AgentSendError, AgentStreamEvent, AgentRuntimeControl, MockAgentRuntime, AgentRuntimeRegistry, InMemoryAgentRuntimeRegistry,
};
use nomifun_api_types::AutoWorkTargetKind;
use nomifun_app::{AppConfig, AppServices, create_router};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, ConversationStatus, ProviderId, TimestampMs, now_ms,
};
use nomifun_db::{IRequirementRepository, SqliteRequirementRepository};
use nomifun_requirement::service::MAX_ATTEMPTS;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

/// A mock Nomi agent that completes any turn cleanly: on `send_message` it emits
/// a benign text (NOT a 选择题/开放式提问 — so IDMM's decision watch stays standby)
/// followed by a clean `Finish`. With no requirement-verdict tool call, AutoWork
/// parks a Nomi turn at `needs_review` — proof the turn actually RAN.
struct CompletingNomiAgent {
    conversation_id: String,
    event_tx: tokio::sync::broadcast::Sender<AgentStreamEvent>,
}

#[async_trait::async_trait]
impl AgentRuntimeControl for CompletingNomiAgent {
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
        Some(ConversationStatus::Running)
    }
    fn is_transport_healthy(&self) -> bool {
        true
    }
    fn last_activity_at(&self) -> TimestampMs {
        now_ms()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
        // A clean, non-decision turn: benign text + Finish(EndTurn). The relay
        // subscribed before this call, so the buffered events are consumed.
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: "已处理该需求，提交复核。".into(),
        }));
        let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
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
impl MockAgentRuntime for CompletingNomiAgent {}

/// Build an app whose agent factory returns a `CompletingNomiAgent`.
async fn build_app_completing() -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let isolated_root = tempfile::tempdir().unwrap().keep();
    let factory: Arc<
        dyn Fn(AgentRuntimeBuildOptions) -> futures_util::future::BoxFuture<'static, Result<AgentRuntimeHandle, AppError>>
            + Send
            + Sync,
    > = Arc::new(move |opts: AgentRuntimeBuildOptions| {
        Box::pin(async move {
            let (event_tx, _) = tokio::sync::broadcast::channel(256);
            Ok(AgentRuntimeHandle::Mock(Arc::new(CompletingNomiAgent {
                conversation_id: opts.conversation_id,
                event_tx,
            })))
        })
    });
    let runtime_registry: Arc<dyn AgentRuntimeRegistry> = Arc::new(InMemoryAgentRuntimeRegistry::new(factory));
    let services = AppServices::from_config(
        db,
        &AppConfig {
            data_dir: isolated_root.join("data"),
            work_dir: isolated_root.join("work"),
            ..AppConfig::default()
        },
    )
        .await
        .unwrap()
        .with_agent_runtime_registry(runtime_registry);
    let router = create_router(&services).await;
    (router, services)
}

/// AutoWork reaches the same production runtime-options boundary as an
/// interactive Nomi turn, so its fixture must carry a real canonical model
/// binding even though the runtime implementation itself is mocked.
async fn seed_mock_provider(services: &AppServices) -> String {
    let provider_id = ProviderId::new().into_string();
    nomifun_db::sqlx::query(
        "INSERT INTO providers \
         (provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
          capabilities, created_at, updated_at) \
         VALUES (?, 'openai', 'AutoWork IDMM mock', 'https://example.invalid', \
                 'encrypted', '[\"mock-model\"]', 1, '[]', 1, 1)",
    )
    .bind(&provider_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    provider_id
}

#[tokio::test]
async fn autowork_and_idmm_enable_auto_resumes_paused_tag_and_runs_requirement() {
    let (mut app, services) = build_app_completing().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = seed_mock_provider(&services).await;
    let tag = "coop";

    // A plain desktop nomi conversation (no channel markers → IDMM may act).
    let conv = {
        let body = json!({
            "type": "nomi",
            "name": "coop",
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

    // One requirement in `tag`.
    let req_id = {
        let body = json!({ "title": "贪吃蛇", "content": "做个贪吃蛇", "tag": tag, "order_key": "1" });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/requirements", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create requirement failed: {}", resp.status());
        body_json(resp).await["data"]["requirement_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // Prior admitted attempts are outside this cooperation test's scope. Seed
    // their durable budget while the row is still pending, then let the real
    // allocator create the final attempt and exercise terminal exhaustion.
    // No-admission failures are atomic pre-effect abandons whose attempts are
    // intentionally refunded, so replaying one cannot manufacture exhaustion.
    let requirement_repo = SqliteRequirementRepository::new(services.database.pool().clone());
    let seeded = sqlx::query(
        "UPDATE requirements SET attempt_count = ?1 \
          WHERE requirement_id = ?2 AND status = 'pending'",
    )
    .bind(MAX_ATTEMPTS - 1)
    .bind(&req_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    assert_eq!(
        seeded.rows_affected(),
        1,
        "pause setup must seed exactly one pending requirement"
    );
    let claim = requirement_repo
        .claim_next_for_runner(tag, Some(&conv), None, 60_000, now_ms())
        .await
        .unwrap()
        .expect("final retry-budget claim must be allocated")
        .row;
    assert_eq!(claim.attempt_count, MAX_ATTEMPTS);
    let claim_token = claim
        .claim_token
        .as_deref()
        .expect("runner claim carries its capability");
    services
        .requirement_service
        .finalize_claim_if_needed(
            &req_id,
            claim.claim_generation,
            claim_token,
            &conv,
            AutoWorkTargetKind::Conversation,
            true,
            None,
            false,
        )
        .await
        .unwrap();
    assert!(
        services.requirement_service.is_tag_paused(tag).await.unwrap(),
        "precondition: the final admitted-attempt budget must pause the tag"
    );

    // Enable 智能决策 (decision watch, rule tier) AND 自动工作 — exactly the user's
    // double-toggle. IDMM stays standby (no decision is emitted); the AutoWork
    // enable must auto-resume the paused tag.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/idmm",
            json!({ "kind": "conversation", "target_id": conv, "decision_watch": { "enabled": true, "tier": "rule_only" } }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "enabling 智能决策 should succeed");

    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "kind": "conversation", "target_id": conv, "enabled": true, "tag": tag }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success(), "enabling 自动工作 should succeed: {}", resp.status());

    // (1) Deterministic: the enable auto-resumed the tag (runs synchronously in the
    // handler before the loop starts).
    assert!(
        !services.requirement_service.is_tag_paused(tag).await.unwrap(),
        "enabling 自动工作 must auto-resume the paused tag (the 彻底不工作 fix)"
    );

    // (2) The two features cooperate: the AutoWork loop claims the now-resumed
    // requirement and runs the turn to a processed terminal state. A clean Nomi
    // turn with no verdict tool call parks at `needs_review` (NOT stuck `failed`).
    let mut last = String::new();
    let mut processed = false;
    for _ in 0..200 {
        let resp = app
            .clone()
            .oneshot(get_with_token(&format!("/api/requirements/{req_id}"), &token))
            .await
            .unwrap();
        last = body_json(resp).await["data"]["status"].as_str().unwrap_or("").to_string();
        if matches!(last.as_str(), "needs_review" | "done") {
            processed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        processed,
        "with 自动工作 + 智能决策 both on, the resumed requirement must be claimed and run to a processed state; last status = {last:?}"
    );
    assert!(
        !services.requirement_service.is_tag_paused(tag).await.unwrap(),
        "the tag must remain resumed after the turn (the clean turn must not re-pause it)"
    );
}

#[tokio::test]
async fn autowork_broadcasts_run_state_transitions_for_session_list_sync() {
    // REGRESSION (用户截图: 顶部「自动工作」图标=绿/active,但侧边栏同一会话=橙/idle):
    // the AutoWork runner updated its in-memory `live_progress` on claim/finish but
    // emitted NO autowork state event, so the session-list capability icon — which
    // updates ONLY from `autowork.statusChanged` (no per-row GET) — kept its stale
    // bulk-loaded run-state while the per-session control (which re-GETs on open)
    // showed the live one. The loop must now BROADCAST run_state=active on claim
    // and run_state=idle on finish so both surfaces land on the same colour.
    let (mut app, services) = build_app_completing().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let provider_id = seed_mock_provider(&services).await;
    let tag = "rs-sync";

    let conv = {
        let body = json!({
            "type": "nomi",
            "name": "rs",
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
    let req_id = {
        let body = json!({ "title": "需求", "content": "做点事", "tag": tag, "order_key": "1" });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/requirements", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create requirement failed: {}", resp.status());
        body_json(resp).await["data"]["requirement_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // Capture owner-scoped events BEFORE enabling, so the loop's emits are
    // observed without weakening private runtime state back into the
    // installation-wide broadcast channel.
    let mut events = services.event_bus.subscribe_user();

    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "kind": "conversation", "target_id": conv, "enabled": true, "tag": tag }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success(), "enabling 自动工作 should succeed: {}", resp.status());

    // Collect this conversation's autowork run-state transitions while the loop
    // claims → the mock completes the turn → finalize.
    let mut run_states: Vec<String> = Vec::new();
    let mut processed = false;
    for _ in 0..240 {
        loop {
            match events.try_recv() {
                Ok(envelope) => {
                    let msg = envelope.event;
                    if msg.name == "autowork.statusChanged"
                        && msg.data.get("target_id").and_then(|v| v.as_str()) == Some(conv.as_str())
                    {
                        assert_eq!(
                            envelope.user_id,
                            services.authoritative_user_id.as_ref(),
                            "AutoWork runtime state must remain scoped to the canonical owner"
                        );
                        if let Some(rs) = msg.data.get("run_state").and_then(|v| v.as_str()) {
                            run_states.push(rs.to_string());
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        let resp = app
            .clone()
            .oneshot(get_with_token(&format!("/api/requirements/{req_id}"), &token))
            .await
            .unwrap();
        let status = body_json(resp).await["data"]["status"].as_str().unwrap_or("").to_string();
        if matches!(status.as_str(), "needs_review" | "done") {
            processed = true;
        }
        // The finish `idle` emit fires just AFTER the status settles, so keep
        // draining until we have seen active and landed back on idle.
        if processed
            && run_states.iter().any(|s| s == "active")
            && run_states.last().map(|s| s == "idle").unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(processed, "the requirement must be claimed and run; run_states seen = {run_states:?}");
    assert!(
        run_states.iter().any(|s| s == "active"),
        "AutoWork must broadcast run_state=active on claim (the session-list sync fix); states = {run_states:?}"
    );
    // After the turn finishes the loop returns to idle and broadcasts it, so the
    // last transition both surfaces observe is idle (header AND sidebar align).
    assert_eq!(
        run_states.last().map(String::as_str),
        Some("idle"),
        "AutoWork must broadcast run_state=idle after the turn finishes; states = {run_states:?}"
    );
}
