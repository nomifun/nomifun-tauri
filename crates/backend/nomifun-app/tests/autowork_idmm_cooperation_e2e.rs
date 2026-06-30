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
use nomifun_ai_agent::types::{BuildTaskOptions, SendMessageData};
use nomifun_ai_agent::{
    AgentInstance, AgentSendError, AgentStreamEvent, IAgentTask, IMockAgent, IWorkerTaskManager, WorkerTaskManagerImpl,
};
use nomifun_api_types::AutoWorkTargetKind;
use nomifun_app::{AppConfig, AppServices, create_router};
use nomifun_common::{AgentKillReason, AgentType, AppError, ConversationStatus, TimestampMs, now_ms};

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
impl IAgentTask for CompletingNomiAgent {
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
impl IMockAgent for CompletingNomiAgent {}

/// Build an app whose agent factory returns a `CompletingNomiAgent`.
async fn build_app_completing() -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let factory: Arc<
        dyn Fn(BuildTaskOptions) -> futures_util::future::BoxFuture<'static, Result<AgentInstance, AppError>>
            + Send
            + Sync,
    > = Arc::new(move |opts: BuildTaskOptions| {
        Box::pin(async move {
            let (event_tx, _) = tokio::sync::broadcast::channel(256);
            Ok(AgentInstance::Mock(Arc::new(CompletingNomiAgent {
                conversation_id: opts.conversation_id,
                event_tx,
            })))
        })
    });
    let wtm: Arc<dyn IWorkerTaskManager> = Arc::new(WorkerTaskManagerImpl::new(factory));
    let services = AppServices::from_config(db, &AppConfig::default())
        .await
        .unwrap()
        .with_worker_task_manager(wtm);
    let router = create_router(&services).await;
    (router, services)
}

#[tokio::test]
async fn autowork_and_idmm_enable_auto_resumes_paused_tag_and_runs_requirement() {
    let (mut app, services) = build_app_completing().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let tag = "coop";

    // A plain desktop nomi conversation (no channel markers → IDMM may act).
    let conv = {
        let body = json!({ "type": "nomi", "name": "coop", "extra": { "workspace": "/project" } });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/conversations", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create conversation failed: {}", resp.status());
        body_json(resp).await["data"]["id"].as_i64().unwrap().to_string()
    };
    let conv_i64: i64 = conv.parse().unwrap();

    // One requirement in `tag`.
    let req_id = {
        let body = json!({ "title": "贪吃蛇", "content": "做个贪吃蛇", "tag": tag, "order_key": "1" });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/requirements", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create requirement failed: {}", resp.status());
        body_json(resp).await["data"]["id"].as_i64().unwrap()
    };

    // Drive the requirement to failure MAX_ATTEMPTS (=3) times so its tag PAUSES —
    // the exact stuck state the user's tag was left in. Done via the service so the
    // precondition is deterministic (no dependency on a failing live turn).
    for _ in 0..3 {
        services
            .requirement_service
            .claim_next(tag, conv_i64, AutoWorkTargetKind::Conversation, 60_000)
            .await
            .unwrap()
            .expect("claimable during pause setup");
        services.requirement_service.finalize_if_needed(req_id, true, None, false).await.unwrap();
    }
    assert!(
        services.requirement_service.is_tag_paused(tag).await.unwrap(),
        "precondition: 3 failures must pause the tag"
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
    // the orchestrator updated its in-memory `live_progress` on claim/finish but
    // emitted NO autowork state event, so the session-list capability icon — which
    // updates ONLY from `autowork.statusChanged` (no per-row GET) — kept its stale
    // bulk-loaded run-state while the per-session control (which re-GETs on open)
    // showed the live one. The loop must now BROADCAST run_state=active on claim
    // and run_state=idle on finish so both surfaces land on the same colour.
    let (mut app, services) = build_app_completing().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let tag = "rs-sync";

    let conv = {
        let body = json!({ "type": "nomi", "name": "rs", "extra": { "workspace": "/project" } });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/conversations", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create conversation failed: {}", resp.status());
        body_json(resp).await["data"]["id"].as_i64().unwrap().to_string()
    };
    let req_id = {
        let body = json!({ "title": "需求", "content": "做点事", "tag": tag, "order_key": "1" });
        let resp = app
            .clone()
            .oneshot(json_with_token("POST", "/api/requirements", body, &token, &csrf))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "create requirement failed: {}", resp.status());
        body_json(resp).await["data"]["id"].as_i64().unwrap()
    };

    // Capture broadcast events BEFORE enabling, so the loop's emits are observed.
    let mut events = services.event_bus.subscribe();

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
                Ok(msg) => {
                    if msg.name == "autowork.statusChanged"
                        && msg.data.get("target_id").and_then(|v| v.as_str()) == Some(conv.as_str())
                        && let Some(rs) = msg.data.get("run_state").and_then(|v| v.as_str())
                    {
                        run_states.push(rs.to_string());
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
