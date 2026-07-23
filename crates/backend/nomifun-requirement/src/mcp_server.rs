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
//! AutoWork runner can park an un-declared clean turn as `needs_review` instead of
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
//! The process-local issuer never leaves this server. Each bridge child receives
//! a renewable lease bootstrap: short-lived access binds user, session, tools,
//! and requirement owner scope, while the renewal proof names no mutable scope.
//! Identity is derived only from verified claims; loose body fields do not exist
//! in the protocol.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use nomifun_api_types::{
    REQUIREMENT_CAPABILITY_DOMAIN, RequirementCapabilityClaims,
    RequirementCapabilityScope, RequirementMcpConfig, RequirementStatus,
};
use nomifun_common::{
    LOOPBACK_CAPABILITY_RENEW_PATH, LOOPBACK_CAPABILITY_REVOKE_PATH,
    LoopbackCapabilityIssuer, LoopbackCapabilityRenewalRequest,
    RequirementId,
};
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
    issuer: Arc<LoopbackCapabilityIssuer>,
    service: ServiceSlot,
}

/// In-process HTTP MCP server for requirement declaration tools.
pub struct RequirementMcpServer {
    http_addr: SocketAddr,
    issuer: Arc<LoopbackCapabilityIssuer>,
    shutdown_handle: Option<tokio::task::JoinHandle<()>>,
    service_slot: ServiceSlot,
}

impl RequirementMcpServer {
    /// Bind a fresh `127.0.0.1:0` listener, create a process-local issuer, and
    /// start serving capability lifecycle routes plus `POST /tool`. The service
    /// must be wired separately via [`set_service`](Self::set_service) before
    /// the first tool call arrives.
    pub async fn start() -> Result<Self, String> {
        let issuer = Arc::new(LoopbackCapabilityIssuer::random()?);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind requirement MCP HTTP listener: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to read requirement MCP local addr: {e}"))?;

        let service_slot: ServiceSlot = Arc::new(RwLock::new(Weak::new()));

        let state = ReqMcpState {
            issuer: issuer.clone(),
            service: service_slot.clone(),
        };

        let app = axum::Router::new()
            .route("/tool", axum::routing::post(handle_tool_request))
            .route(
                LOOPBACK_CAPABILITY_RENEW_PATH,
                axum::routing::post(handle_capability_renew),
            )
            .route(
                LOOPBACK_CAPABILITY_REVOKE_PATH,
                axum::routing::post(handle_capability_revoke),
            )
            .with_state(state);

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "Requirement MCP axum server exited with error");
            }
        });

        debug!(http_port = http_addr.port(), "Requirement MCP Server started (axum)");

        Ok(Self {
            http_addr,
            issuer,
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

    /// Build the process-private issuer config consumed by the Agent/Terminal
    /// assemblers. It is deliberately non-serializable and redacts its issuer.
    pub fn issuer_config(&self, binary_path: String) -> RequirementMcpConfig {
        RequirementMcpConfig::from_issuer(
            self.http_addr.port(),
            self.issuer.clone(),
            binary_path,
        )
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
    let presented_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    let claims: RequirementCapabilityClaims = match body
        .get("session")
        .cloned()
        .and_then(|value| serde_json::from_value::<RequirementCapabilityClaims>(value).ok())
    {
        Some(claims)
            if state
                .issuer
                .verify_access(
                    REQUIREMENT_CAPABILITY_DOMAIN,
                    &claims,
                    presented_token,
                )
                .is_ok()
                && claims.scope.validate(&claims.session).is_ok() => claims,
        _ => {
            warn!("Requirement MCP: rejected invalid, expired, or missing scoped capability");
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})))
                .into_response();
        }
    };

    let tool = body.get("tool").and_then(Value::as_str).unwrap_or("");
    if !claims.allows(tool) {
        warn!(tool, "Requirement MCP: tool is outside signed capability scope");
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"})))
            .into_response();
    }
    let args = body.get("args").cloned().unwrap_or(Value::Null);

    let svc = match state.service.read().await.upgrade() {
        Some(s) => s,
        None => {
            warn!(tool, "Requirement MCP: service not available");
            return finish(json!({"error": "service_unavailable"}));
        }
    };

    info!(tool, "Requirement MCP: dispatching tool");

    let response_body = match tool {
        "requirement_complete" => exec_complete(&svc, &args, &claims).await,
        "requirement_update_status" => exec_update_status(&svc, &args, &claims).await,
        unknown => {
            warn!(tool = unknown, "Requirement MCP: unknown tool");
            json!({"error": format!("Unknown tool: {unknown}")})
        }
    };

    finish(response_body)
}

/// Renew a short-lived access credential from the process-local immutable
/// authorization. The bridge never submits user/session/tool/scope fields.
async fn handle_capability_renew(
    State(state): State<ReqMcpState>,
    Json(request): Json<LoopbackCapabilityRenewalRequest>,
) -> axum::response::Response {
    match state
        .issuer
        .renew::<RequirementCapabilityScope>(REQUIREMENT_CAPABILITY_DOMAIN, &request)
    {
        Ok(access) if access.claims.scope.validate(&access.claims.session).is_ok() => {
            Json(access).into_response()
        }
        _ => {
            warn!("Requirement MCP: rejected invalid capability renewal");
            (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})))
                .into_response()
        }
    }
}

/// Explicit child/runtime teardown. Invalid proofs fail closed; callers treat
/// transport failure as best-effort because process exit also destroys the
/// in-memory registry.
async fn handle_capability_revoke(
    State(state): State<ReqMcpState>,
    Json(request): Json<LoopbackCapabilityRenewalRequest>,
) -> axum::response::Response {
    match state
        .issuer
        .revoke(REQUIREMENT_CAPABILITY_DOMAIN, &request)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => {
            warn!("Requirement MCP: rejected invalid capability revocation");
            (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"})))
                .into_response()
        }
    }
}

fn json_to_requirement_id(v: Option<&Value>) -> Option<String> {
    let value = v?.as_str()?;
    RequirementId::parse(value).ok()?;
    Some(value.to_owned())
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

async fn exec_complete(
    svc: &RequirementService,
    args: &Value,
    claims: &RequirementCapabilityClaims,
) -> Value {
    let id = match json_to_requirement_id(args.get("id")) {
        Some(id) => id,
        None => return json!({"error": "missing or invalid canonical requirement id"}),
    };
    let note = args
        .get("completion_note")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Err(e) = verify_scope(svc, &id, claims).await {
        return json!({"error": e});
    }
    match svc.complete(&id, note).await {
        Ok(_) => {
            info!(requirement_id = %id, "Requirement MCP: requirement_complete succeeded");
            json!({"result": format!("Requirement {id} marked complete.")})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn exec_update_status(
    svc: &RequirementService,
    args: &Value,
    claims: &RequirementCapabilityClaims,
) -> Value {
    let id = match json_to_requirement_id(args.get("id")) {
        Some(id) => id,
        None => return json!({"error": "missing or invalid canonical requirement id"}),
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
    if let Err(e) = verify_scope(svc, &id, claims).await {
        return json!({"error": e});
    }
    match svc.set_status(&id, status, note).await {
        Ok(_) => {
            info!(requirement_id = %id, status = status_str, "Requirement MCP: requirement_update_status succeeded");
            json!({"result": format!("Requirement {id} status set to {status_str}.")})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

/// A declaration child can mutate only work currently owned by the exact signed
/// session and domain. Unowned work is rejected: the old caller-id-absent and
/// unclaimed fallbacks made an authenticated process-wide token a global write
/// capability.
async fn verify_scope(
    svc: &RequirementService,
    id: &str,
    claims: &RequirementCapabilityClaims,
) -> Result<(), String> {
    let caller_kind = claims.scope.owner_kind.as_str();
    let caller_id = claims.scope.owner_session_id.as_str();
    if claims.session.kind != claims.scope.owner_kind
        || claims.session.session_id != caller_id
    {
        return Err("signed requirement scope is internally inconsistent".into());
    }
    let req = svc.get(id).await.map_err(|e| e.to_string())?;
    match caller_kind {
        "conversation" if req.owner_conversation_id.as_deref() == Some(caller_id) => Ok(()),
        "terminal" if req.owner_terminal_id.as_deref() == Some(caller_id) => Ok(()),
        _ => Err(format!("requirement {id} is owned by a different session")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RequirementEventEmitter;
    use nomifun_api_types::{AutoWorkTargetKind, CreateRequirementRequest};
    use nomifun_common::{ConversationId, TerminalId};
    use nomifun_db::{SqliteRequirementRepository, init_database_memory};
    use nomifun_realtime::UserEventSink;

    #[derive(Default)]
    struct NoopBroadcaster;
    impl UserEventSink for NoopBroadcaster {
        fn send_to_user(
            &self,
            _user_id: &str,
            _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
        ) {
        }
    }

    async fn service_with_claim(
        kind: AutoWorkTargetKind,
    ) -> (Arc<RequirementService>, String, String, String) {
        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo: Arc<dyn nomifun_db::IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(db.pool().clone()));
        let owner_id = match kind {
            AutoWorkTargetKind::Conversation => {
                let id = ConversationId::new().into_string();
                sqlx::query(
                    "INSERT INTO conversations \
                        (conversation_id, user_id, name, type, created_at, updated_at) \
                     VALUES (?1, ?2, 'Requirement MCP Conversation', 'nomi', 0, 0)",
                )
                .bind(&id)
                .bind(&installation_owner)
                .execute(db.pool())
                .await
                .unwrap();
                id
            }
            AutoWorkTargetKind::Terminal => {
                let id = TerminalId::new().into_string();
                sqlx::query(
                    "INSERT INTO terminal_sessions \
                        (terminal_id, user_id, name, cwd, command, args, cols, rows, last_status, created_at, updated_at) \
                     VALUES (?1, ?2, 'Requirement MCP Terminal', '/tmp', '$SHELL', '[]', 80, 24, 'running', 0, 0)",
                )
                .bind(&id)
                .bind(&installation_owner)
                .execute(db.pool())
                .await
                .unwrap();
                id
            }
        };
        let emitter = RequirementEventEmitter::new(
            Arc::new(NoopBroadcaster),
            Arc::from(installation_owner.as_str()),
        );
        let service = Arc::new(RequirementService::new(repo, emitter));
        let requirement = service
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
        service
            .claim_next("t", &owner_id, kind, 120_000)
            .await
            .unwrap()
            .unwrap();
        Box::leak(Box::new(db));
        (
            service,
            installation_owner,
            owner_id,
            requirement.requirement_id,
        )
    }

    fn child_for(
        server: &RequirementMcpServer,
        installation_owner: &str,
        kind: AutoWorkTargetKind,
        owner_id: &str,
    ) -> nomifun_api_types::RequirementMcpChildConfig {
        let config = server.issuer_config("/bin/nomicore".into());
        match kind {
            AutoWorkTargetKind::Conversation => config
                .issue_for_conversation(installation_owner, owner_id)
                .unwrap(),
            AutoWorkTargetKind::Terminal => {
                config.issue_for_terminal(installation_owner, owner_id).unwrap()
            }
        }
    }

    async fn post_tool(
        server: &RequirementMcpServer,
        child: &nomifun_api_types::RequirementMcpChildConfig,
        tool: &str,
        args: Value,
    ) -> (u16, Value) {
        let claims = &child.bootstrap.access.claims;
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header(
                "Authorization",
                format!("Bearer {}", child.bootstrap.access.token),
            )
            .json(&json!({
                "tool": tool,
                "args": args,
                "session": claims,
            }))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    #[test]
    fn requirement_id_parser_accepts_only_canonical_uuidv7_strings() {
        let id = RequirementId::new().into_string();
        assert_eq!(
            json_to_requirement_id(Some(&json!(id))),
            Some(id.clone())
        );
        assert!(json_to_requirement_id(Some(&json!(7))).is_none());
        assert!(json_to_requirement_id(Some(&json!("7"))).is_none());
        assert!(json_to_requirement_id(Some(&json!(format!("requirement_{id}")))).is_none());
    }

    #[tokio::test]
    async fn conversation_child_completes_only_its_owned_requirement() {
        let (service, installation_owner, owner_id, requirement_id) =
            service_with_claim(AutoWorkTargetKind::Conversation).await;
        let server = RequirementMcpServer::start().await.unwrap();
        server.set_service(Arc::downgrade(&service)).await;
        let child = child_for(
            &server,
            &installation_owner,
            AutoWorkTargetKind::Conversation,
            &owner_id,
        );

        let (status, body) = post_tool(
            &server,
            &child,
            "requirement_complete",
            json!({"id": requirement_id, "completion_note": "done"}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(body.get("result").is_some(), "{body}");
        let row = service.get(&requirement_id).await.unwrap();
        assert_eq!(row.status, RequirementStatus::Done);
    }

    #[tokio::test]
    async fn numeric_requirement_id_is_rejected_without_mutation() {
        let (service, installation_owner, owner_id, requirement_id) =
            service_with_claim(AutoWorkTargetKind::Conversation).await;
        let server = RequirementMcpServer::start().await.unwrap();
        server.set_service(Arc::downgrade(&service)).await;
        let child = child_for(
            &server,
            &installation_owner,
            AutoWorkTargetKind::Conversation,
            &owner_id,
        );

        let (status, body) = post_tool(
            &server,
            &child,
            "requirement_complete",
            json!({"id": 1}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            body.get("error").and_then(Value::as_str),
            Some("missing or invalid canonical requirement id")
        );
        assert_eq!(
            service.get(&requirement_id).await.unwrap().status,
            RequirementStatus::InProgress
        );
    }

    #[tokio::test]
    async fn cross_domain_child_is_denied() {
        let (service, installation_owner, terminal_id, requirement_id) =
            service_with_claim(AutoWorkTargetKind::Terminal).await;
        let conversation_id = ConversationId::new().into_string();
        let server = RequirementMcpServer::start().await.unwrap();
        server.set_service(Arc::downgrade(&service)).await;
        let child = child_for(
            &server,
            &installation_owner,
            AutoWorkTargetKind::Conversation,
            &conversation_id,
        );

        let (status, body) = post_tool(
            &server,
            &child,
            "requirement_complete",
            json!({"id": requirement_id}),
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            body.get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("different session")),
            "{body}"
        );
        let row = service.get(&requirement_id).await.unwrap();
        assert_eq!(row.status, RequirementStatus::InProgress);
        assert_eq!(row.owner_terminal_id.as_deref(), Some(terminal_id.as_str()));
    }

    #[tokio::test]
    async fn missing_or_tampered_capability_is_unauthorized() {
        let (service, installation_owner, owner_id, requirement_id) =
            service_with_claim(AutoWorkTargetKind::Conversation).await;
        let server = RequirementMcpServer::start().await.unwrap();
        server.set_service(Arc::downgrade(&service)).await;
        let child = child_for(
            &server,
            &installation_owner,
            AutoWorkTargetKind::Conversation,
            &owner_id,
        );

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .json(&json!({
                "tool": "requirement_complete",
                "args": {"id": requirement_id},
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);

        let mut claims = child.bootstrap.access.claims.clone();
        claims.scope.owner_session_id = ConversationId::new().into_string();
        let response = client
            .post(format!("http://127.0.0.1:{}/tool", server.http_port()))
            .header(
                "Authorization",
                format!("Bearer {}", child.bootstrap.access.token),
            )
            .json(&json!({
                "tool": "requirement_complete",
                "args": {"id": requirement_id},
                "session": claims,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);
    }
}
