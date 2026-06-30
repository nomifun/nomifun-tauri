//! In-process HTTP MCP server exposing requirement *declaration* tools to ACP
//! agent sessions (claude / codex / gemini CLIs).
//!
//! ## Why this exists
//!
//! AutoWork drives ACP sessions, but ACP CLIs have no in-process tool bus we
//! can register `RequirementCompleteTool` into (only the nomi engine does).
//! Without a declaration channel, a clean turn that did NOT actually finish the
//! requirement is silently recorded as `done` — the original "失败却标成成功"
//! bug. This server gives ACP agents the SAME `requirement_complete` /
//! `requirement_update_status` surface the nomi engine has natively, so the
//! orchestrator can park an un-declared clean turn as `needs_review` instead of
//! assuming success (`expects_verdict`).
//!
//! This is the in-process HTTP half. ACP CLIs spawn a SEPARATE stdio process
//! (`nomicore mcp-requirement-stdio`) that cannot share this process's
//! `RequirementService`; it forwards each tool call back here as an
//! authenticated `POST /tool`. The transport is stdio because claude / codex /
//! gemini advertise stdio-only MCP capabilities (HTTP/SSE servers are dropped
//! by the ACP capability filter), so a direct-HTTP injection would never reach
//! them.
//!
//! ## Security
//!
//! A random opaque bearer token gates every request (per-process, like the
//! guide server). On top of that, `verify_scope` refuses to mutate a
//! requirement owned by a *different* conversation than the calling session —
//! defense-in-depth so a stale/incorrect id cannot let one AutoWork session
//! complete another's requirement.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use nomifun_api_types::RequirementStatus;
use nomifun_common::generate_id;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::service::RequirementService;

/// Late-bound handle to the singleton `RequirementService`. Held as a `Weak` so
/// the server never keeps the service alive on its own (matches the guide
/// server's slot pattern). Wired via [`RequirementMcpServer::set_service`].
type ServiceSlot = Arc<RwLock<Weak<RequirementService>>>;

#[derive(Clone)]
struct ReqMcpState {
    auth_token: String,
    service: ServiceSlot,
}

/// In-process HTTP MCP server for requirement declaration tools.
pub struct RequirementMcpServer {
    http_addr: SocketAddr,
    auth_token: String,
    shutdown_handle: Option<tokio::task::JoinHandle<()>>,
    service_slot: ServiceSlot,
}

impl RequirementMcpServer {
    /// Bind a fresh `127.0.0.1:0` listener, mint a random bearer token, and
    /// start serving `POST /tool`. The service must be wired separately via
    /// [`set_service`](Self::set_service) before the first tool call arrives.
    pub async fn start() -> Result<Self, String> {
        let auth_token = generate_id();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind requirement MCP HTTP listener: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to read requirement MCP local addr: {e}"))?;

        let service_slot: ServiceSlot = Arc::new(RwLock::new(Weak::new()));

        let state = ReqMcpState {
            auth_token: auth_token.clone(),
            service: service_slot.clone(),
        };

        let app = axum::Router::new()
            .route("/tool", axum::routing::post(handle_tool_request))
            .with_state(state);

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "Requirement MCP axum server exited with error");
            }
        });

        debug!(http_port = http_addr.port(), "Requirement MCP Server started (axum)");

        Ok(Self {
            http_addr,
            auth_token,
            shutdown_handle: Some(handle),
            service_slot,
        })
    }

    /// Wire the singleton `RequirementService` after it is constructed. Must be
    /// called once before the first tool request arrives.
    pub async fn set_service(&self, service: Weak<RequirementService>) {
        *self.service_slot.write().await = service;
    }

    pub fn http_port(&self) -> u16 {
        self.http_addr.port()
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn stop(&mut self) {
        if let Some(handle) = self.shutdown_handle.take() {
            handle.abort();
            debug!(http_port = self.http_addr.port(), "Requirement MCP Server stop requested");
        }
    }
}

impl Drop for RequirementMcpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Axum handler
// ---------------------------------------------------------------------------

async fn handle_tool_request(
    State(state): State<ReqMcpState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let provided_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if provided_token != state.auth_token {
        warn!("Requirement MCP: unauthorized request");
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }

    let tool = body.get("tool").and_then(Value::as_str).unwrap_or("");
    let args = body.get("args").cloned().unwrap_or(Value::Null);
    // The caller's conversation id is an integer (single-track, spec §2.3). It is
    // tolerated as a JSON number OR a numeric string (the stdio bridge forwards
    // the `ENV_CONVERSATION_ID` env value, which is a string). `None` = no
    // conversation context → `verify_scope` is lenient (single-session / tests).
    let caller_conv = json_to_i64(body.get("conversation_id"));
    // owner_kind: "conversation" (default, back-compat when field absent) or
    // "terminal". Controls cross-domain scope check in verify_scope.
    let caller_kind = body
        .get("owner_kind")
        .and_then(Value::as_str)
        .unwrap_or("conversation");

    let svc = match state.service.read().await.upgrade() {
        Some(s) => s,
        None => {
            warn!(tool, "Requirement MCP: service not available");
            return finish(json!({"error": "service_unavailable"}));
        }
    };

    info!(tool, "Requirement MCP: dispatching tool");

    let response_body = match tool {
        "requirement_complete" => exec_complete(&svc, &args, caller_conv, caller_kind).await,
        "requirement_update_status" => exec_update_status(&svc, &args, caller_conv, caller_kind).await,
        unknown => {
            warn!(tool = unknown, "Requirement MCP: unknown tool");
            json!({"error": format!("Unknown tool: {unknown}")})
        }
    };

    finish(response_body)
}

/// Extract an integer id from a JSON value, tolerating both a JSON number and a
/// numeric string (agents occasionally stringify; the stdio bridge forwards the
/// env-sourced `conversation_id` as a string). Returns `None` for absent / null
/// / non-numeric — the caller decides whether that is benign.
fn json_to_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Wrap a JSON body as a response and ask the client to close the connection
/// (the stdio bridge runs with `pool_max_idle_per_host(0)` and does not reuse).
fn finish(body: Value) -> axum::response::Response {
    let mut resp = Json(body).into_response();
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

async fn exec_complete(svc: &RequirementService, args: &Value, caller_id: Option<i64>, caller_kind: &str) -> Value {
    let id = match json_to_i64(args.get("id")) {
        Some(id) => id,
        None => return json!({"error": "missing or non-integer required field: id"}),
    };
    let note = args
        .get("completion_note")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Err(e) = verify_scope(svc, id, caller_id, caller_kind).await {
        return json!({"error": e});
    }
    match svc.complete(id, note).await {
        Ok(_) => {
            info!(requirement_id = id, "Requirement MCP: requirement_complete succeeded");
            json!({"result": format!("Requirement {id} marked complete.")})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn exec_update_status(svc: &RequirementService, args: &Value, caller_id: Option<i64>, caller_kind: &str) -> Value {
    let id = match json_to_i64(args.get("id")) {
        Some(id) => id,
        None => return json!({"error": "missing or non-integer required field: id"}),
    };
    let status_str = args.get("status").and_then(Value::as_str).unwrap_or("");
    let status = match status_str {
        "in_progress" => RequirementStatus::InProgress,
        "done" => RequirementStatus::Done,
        "failed" => RequirementStatus::Failed,
        other => {
            return json!({
                "error": format!("invalid status '{other}' (expected one of: in_progress, done, failed)")
            });
        }
    };
    let note = args
        .get("note")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Err(e) = verify_scope(svc, id, caller_id, caller_kind).await {
        return json!({"error": e});
    }
    match svc.set_status(id, status, note).await {
        Ok(_) => {
            info!(requirement_id = id, status = status_str, "Requirement MCP: requirement_update_status succeeded");
            json!({"result": format!("Requirement {id} status set to {status_str}.")})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

/// Defense-in-depth: a session may only mutate a requirement that is (a) not
/// bound to any session, or (b) bound to THIS caller in the SAME domain.
/// Lenient only when the caller carries no id (`None`) so single-session / test
/// setups never trip it.
///
/// SECURITY (C1, spec §2.2 + Plan 3 D1): the `caller_kind` is sourced from the
/// env `NOMI_REQ_MCP_OWNER_KIND` baked at bridge spawn (not from the agent
/// model), so it is trustworthy. Rules:
///   - caller_id absent → Ok (lenient: single-session / tests)
///   - owner unset → Ok (unclaimed work is mutable by anyone)
///   - owner is conversation AND caller_kind=="conversation" AND same id → Ok
///   - owner is terminal AND caller_kind=="terminal" AND same id → Ok (NEW)
///   - everything else → Err (cross-domain, cross-id, or kind mismatch)
///
/// A terminal can NEVER complete a conversation-owned req, a conversation can
/// NEVER complete a terminal-owned req, and neither can complete a req owned by
/// a different session of its own kind.
async fn verify_scope(
    svc: &RequirementService,
    id: i64,
    caller_id: Option<i64>,
    caller_kind: &str,
) -> Result<(), String> {
    let Some(caller_id) = caller_id else {
        return Ok(());
    };
    let req = svc.get(id).await.map_err(|e| e.to_string())?;
    match (req.owner_session_id, req.owner_kind.as_deref()) {
        // Unowned work is mutable by anyone.
        (None, _) => Ok(()),
        // Owned by a conversation AND caller is a conversation with the same id.
        (Some(owner), Some("conversation")) if caller_kind == "conversation" && owner == caller_id => Ok(()),
        // Owned by a terminal AND caller is a terminal with the same id.
        (Some(owner), Some("terminal")) if caller_kind == "terminal" && owner == caller_id => Ok(()),
        // Everything else: cross-domain, cross-id, or kind mismatch → denied.
        _ => Err(format!("requirement {id} is owned by a different session")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RequirementEventEmitter;
    use nomifun_api_types::{AutoWorkTargetKind, CreateRequirementRequest, RequirementStatus};
    use nomifun_db::{SqliteRequirementRepository, init_database_memory};
    use nomifun_realtime::EventBroadcaster;

    #[derive(Default)]
    struct NoopBroadcaster;
    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _event: nomifun_api_types::WebSocketMessage<serde_json::Value>) {}
    }

    /// Build a service with one requirement in tag `t`, claimed into `conv_1`
    /// (so it is `in_progress` with `conversation_id = conv_1`). Returns the
    /// service (keep it alive — the server holds only a `Weak`) and the req id.
    async fn service_with_claimed_req() -> (Arc<RequirementService>, i64) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn nomifun_db::IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(Arc::new(NoopBroadcaster));
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user_1', 'tester', 'hash', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (1, 'user_1', 'Test Conv', 'acp', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        Box::leak(Box::new(db));
        let svc = Arc::new(RequirementService::new(repo, emitter));
        let req = svc
            .create(CreateRequirementRequest {
                title: "Do X".into(),
                content: "body".into(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = svc
            .claim_next("t", 1, AutoWorkTargetKind::Conversation, 120_000)
            .await
            .unwrap()
            .expect("a pending requirement should be claimable");
        assert_eq!(claimed.id, req.id);
        (svc, req.id)
    }

    async fn started_server(svc: &Arc<RequirementService>) -> RequirementMcpServer {
        let server = RequirementMcpServer::start().await.expect("start");
        server.set_service(Arc::downgrade(svc)).await;
        server
    }

    async fn post_tool(port: u16, token: Option<&str>, body: Value) -> (u16, Value) {
        let client = reqwest::Client::new();
        let mut req = client.post(format!("http://127.0.0.1:{port}/tool")).json(&body);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let json: Value = resp.json().await.unwrap_or(Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn start_returns_positive_port_and_token() {
        let server = RequirementMcpServer::start().await.unwrap();
        assert!(server.http_port() > 0);
        assert!(!server.auth_token().is_empty());
    }

    #[tokio::test]
    async fn each_start_uses_a_fresh_auth_token() {
        let a = RequirementMcpServer::start().await.unwrap();
        let b = RequirementMcpServer::start().await.unwrap();
        assert_ne!(a.auth_token(), b.auth_token());
    }

    #[tokio::test]
    async fn tool_call_requires_auth() {
        let (svc, _id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        let (status, _) = post_tool(
            server.http_port(),
            None,
            json!({"tool": "requirement_complete", "args": {"id": "x"}}),
        )
        .await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn complete_marks_requirement_done() {
        let (svc, id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        let (status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_complete",
                "conversation_id": 1,
                "args": {"id": id, "completion_note": "did the thing"},
            }),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.get("result").is_some(), "expected result, got {body}");
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Done);
        assert_eq!(after.completion_note.as_deref(), Some("did the thing"));
    }

    #[tokio::test]
    async fn update_status_failed_marks_failed() {
        let (svc, id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        let (status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_update_status",
                "conversation_id": 1,
                "args": {"id": id, "status": "failed", "note": "could not finish"},
            }),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.get("result").is_some(), "expected result, got {body}");
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Failed);
    }

    #[tokio::test]
    async fn update_status_rejects_invalid_status() {
        let (svc, id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        let (_status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_update_status",
                "conversation_id": 1,
                "args": {"id": id, "status": "bogus"},
            }),
        )
        .await;
        assert!(
            body.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("bogus")),
            "expected an invalid-status error, got {body}"
        );
        // The requirement must remain untouched (still in_progress).
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::InProgress);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let (svc, _id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        let (status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({"tool": "requirement_explode", "args": {}}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            body.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("Unknown tool")),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn missing_service_returns_unavailable() {
        // Server started but set_service never called → Weak upgrades to None.
        let server = RequirementMcpServer::start().await.unwrap();
        let (status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({"tool": "requirement_complete", "args": {"id": "x"}}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body.get("error").and_then(Value::as_str), Some("service_unavailable"));
    }

    #[tokio::test]
    async fn complete_rejects_cross_session() {
        let (svc, id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        // Requirement is owned by conv_1; a call from conv_other must be refused.
        let (_status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_complete",
                "conversation_id": 2,
                "args": {"id": id, "completion_note": "sneaky"},
            }),
        )
        .await;
        assert!(
            body.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("different session")),
            "expected a cross-session refusal, got {body}"
        );
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::InProgress, "must not be mutated");
    }

    // ── C1 (spec §2.2): cross-domain authz isolation ────────────────────────
    //
    // The requirement MCP caller is ALWAYS a conversation (the MCP is injected
    // into ACP conversation sessions). After integerization `conv#5` and
    // `term#5` share the numeric owner value `5`. A conversation caller must
    // NEVER be allowed to mutate a requirement owned by a TERMINAL that merely
    // shares its number — `verify_scope` pairs the owner with `owner_kind`.

    /// Service with one requirement claimed by TERMINAL #5, plus a conversation
    /// #5 present (same number, different domain). Returns the service + req id.
    async fn service_with_terminal5_claimed_req() -> (Arc<RequirementService>, i64) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn nomifun_db::IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let emitter = RequirementEventEmitter::new(Arc::new(NoopBroadcaster));
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user_1', 'tester', 'hash', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (5, 'user_1', 'Conv Five', 'acp', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO terminal_sessions \
                 (id, user_id, name, cwd, command, args, cols, rows, last_status, created_at, updated_at) \
             VALUES (5, 'user_1', 'Term Five', '/tmp', 'bash', '[]', 80, 24, 'running', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        Box::leak(Box::new(db));
        let svc = Arc::new(RequirementService::new(repo, emitter));
        let req = svc
            .create(CreateRequirementRequest {
                title: "Term work".into(),
                content: "body".into(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .unwrap();
        let claimed = svc
            .claim_next("t", 5, AutoWorkTargetKind::Terminal, 120_000)
            .await
            .unwrap()
            .expect("claimable");
        assert_eq!(claimed.owner_kind.as_deref(), Some("terminal"));
        (svc, req.id)
    }

    #[tokio::test]
    async fn c1_verify_scope_rejects_conversation_caller_for_terminal_owned_req() {
        let (svc, id) = service_with_terminal5_claimed_req().await;
        // Conversation caller #5 — numerically equal to the terminal owner #5.
        let result = verify_scope(&svc, id, Some(5), "conversation").await;
        assert!(
            result.is_err(),
            "conversation #5 must be denied on a terminal#5-owned requirement (cross-domain)"
        );
    }

    #[tokio::test]
    async fn c1_terminal_owned_req_unmutated_by_conversation_mcp_call() {
        // End-to-end through the HTTP tool: a conversation #5 caller's
        // requirement_complete on a terminal#5-owned requirement is refused and
        // the requirement is left in_progress.
        let (svc, id) = service_with_terminal5_claimed_req().await;
        let server = started_server(&svc).await;
        let (_status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_complete",
                "conversation_id": 5,
                "args": {"id": id, "completion_note": "cross-domain"},
            }),
        )
        .await;
        assert!(
            body.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("different session")),
            "expected a cross-domain refusal, got {body}"
        );
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::InProgress, "terminal#5 work must not be mutated by conv#5");
        assert_eq!(after.owner_kind.as_deref(), Some("terminal"));
    }

    // ── Plan 3 Task 1: terminal-caller verify_scope tests ───────────────────

    #[tokio::test]
    async fn terminal_caller_can_complete_own_terminal_owned_req() {
        // (a) term-owned #5 + caller(terminal, 5) → Ok
        let (svc, id) = service_with_terminal5_claimed_req().await;
        let result = verify_scope(&svc, id, Some(5), "terminal").await;
        assert!(result.is_ok(), "terminal #5 must be allowed to complete its own requirement");
    }

    #[tokio::test]
    async fn terminal_caller_cannot_complete_different_terminal_owned_req() {
        // (b) term-owned #5 + caller(terminal, 99) → Err
        let (svc, id) = service_with_terminal5_claimed_req().await;
        let result = verify_scope(&svc, id, Some(99), "terminal").await;
        assert!(
            result.is_err(),
            "terminal #99 must be denied on terminal#5-owned requirement (cross-session)"
        );
    }

    #[tokio::test]
    async fn terminal_caller_cannot_complete_conversation_owned_req() {
        // (e) conv-owned #1 + caller(terminal, 1) → Err
        let (svc, id) = service_with_claimed_req().await;
        // claimed_req is owned by conversation #1
        let result = verify_scope(&svc, id, Some(1), "terminal").await;
        assert!(
            result.is_err(),
            "terminal #1 must be denied on conversation#1-owned requirement (cross-domain)"
        );
    }

    #[tokio::test]
    async fn conversation_caller_can_complete_own_conversation_owned_req() {
        // (d) conv-owned #1 + caller(conversation, 1) → Ok (back-compat)
        let (svc, id) = service_with_claimed_req().await;
        let result = verify_scope(&svc, id, Some(1), "conversation").await;
        assert!(result.is_ok(), "conversation #1 must be allowed to complete its own requirement");
    }

    #[tokio::test]
    async fn absent_owner_kind_defaults_to_conversation_backcompat() {
        // When "owner_kind" is absent from the body (old bridge version), the
        // server defaults to "conversation" for full back-compatibility.
        let (svc, id) = service_with_claimed_req().await;
        let server = started_server(&svc).await;
        // No "owner_kind" field in body — old-style request.
        let (_status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_complete",
                "conversation_id": 1,
                "args": {"id": id, "completion_note": "backcompat"},
            }),
        )
        .await;
        assert!(
            body.get("result").is_some(),
            "absent owner_kind should default to conversation and allow same-id completion, got {body}"
        );
    }

    #[tokio::test]
    async fn terminal_caller_complete_via_http_with_owner_kind() {
        // End-to-end: terminal #5 calls requirement_complete with owner_kind=terminal.
        let (svc, id) = service_with_terminal5_claimed_req().await;
        let server = started_server(&svc).await;
        let (_status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_complete",
                "conversation_id": 5,
                "owner_kind": "terminal",
                "args": {"id": id, "completion_note": "terminal did it"},
            }),
        )
        .await;
        assert!(
            body.get("result").is_some(),
            "terminal #5 with owner_kind=terminal should complete its own req, got {body}"
        );
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::Done);
        assert_eq!(after.completion_note.as_deref(), Some("terminal did it"));
    }

    #[tokio::test]
    async fn different_terminal_caller_denied_via_http() {
        // End-to-end: terminal #99 cannot complete terminal#5-owned req.
        let (svc, id) = service_with_terminal5_claimed_req().await;
        let server = started_server(&svc).await;
        let (_status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({
                "tool": "requirement_complete",
                "conversation_id": 99,
                "owner_kind": "terminal",
                "args": {"id": id, "completion_note": "sneaky terminal"},
            }),
        )
        .await;
        assert!(
            body.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("different session")),
            "terminal #99 must be denied on terminal#5-owned requirement, got {body}"
        );
        let after = svc.get(id).await.unwrap();
        assert_eq!(after.status, RequirementStatus::InProgress, "must not be mutated");
    }
}
