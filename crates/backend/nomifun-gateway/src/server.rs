//! In-process HTTP half of the Platform Gateway MCP.
//!
//! ACP CLIs and the nomi engine spawn a SEPARATE stdio process
//! (`nomicore mcp-gateway-stdio`) that cannot share this process's services;
//! it forwards each tool call back here as an authenticated `POST /tool`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use nomifun_api_types::{
    GATEWAY_CALL_TOOL_OPERATION, GATEWAY_CAPABILITY_DOMAIN,
    GatewayCapabilityClaims, GatewayCapabilityScope, GatewayMcpConfig,
};
use nomifun_common::{
    LOOPBACK_CAPABILITY_RENEW_PATH, LOOPBACK_CAPABILITY_REVOKE_PATH,
    LoopbackCapabilityIssuer, LoopbackCapabilityRenewalRequest, LoopbackSessionKind,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::deps::{CallerCtx, GatewayDeps};
use crate::registry::Registry;

/// Late-bound handle to the gateway dependencies. Unlike the guide /
/// requirement servers (which hold a `Weak` to a singleton that outlives
/// them elsewhere), this slot OWNS the deps bundle: `GatewayDeps` is
/// assembled specifically for this server during router construction and has
/// no other owner. Nothing inside the bundle references the server back, so
/// there is no Arc cycle.
type DepsSlot = Arc<RwLock<Option<Arc<GatewayDeps>>>>;

#[derive(Clone)]
struct GatewayState {
    issuer: Arc<LoopbackCapabilityIssuer>,
    deps: DepsSlot,
}

/// In-process HTTP MCP server for the Platform Gateway tools.
pub struct GatewayMcpServer {
    http_addr: SocketAddr,
    issuer: Arc<LoopbackCapabilityIssuer>,
    shutdown_handle: Option<tokio::task::JoinHandle<()>>,
    deps_slot: DepsSlot,
}

impl GatewayMcpServer {
    /// Bind a fresh `127.0.0.1:0` listener, mint a root issuer secret, and
    /// start serving `POST /tool`. Deps must be wired separately via
    /// [`set_deps`](Self::set_deps) before the first tool call arrives.
    pub async fn start() -> Result<Self, String> {
        let issuer = Arc::new(LoopbackCapabilityIssuer::random()?);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind gateway MCP HTTP listener: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to read gateway MCP local addr: {e}"))?;

        let deps_slot: DepsSlot = Arc::new(RwLock::new(None));

        let state = GatewayState {
            issuer: issuer.clone(),
            deps: deps_slot.clone(),
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
                warn!(error = %e, "Gateway MCP axum server exited with error");
            }
        });

        debug!(
            http_port = http_addr.port(),
            "Gateway MCP Server started (axum)"
        );

        Ok(Self {
            http_addr,
            issuer,
            shutdown_handle: Some(handle),
            deps_slot,
        })
    }

    /// Wire the dependency bundle after router construction. Must be called
    /// once before the first tool request arrives.
    pub async fn set_deps(&self, deps: Arc<GatewayDeps>) {
        *self.deps_slot.write().await = Some(deps);
    }

    pub fn http_port(&self) -> u16 {
        self.http_addr.port()
    }

    /// Build the process-private issuer consumed by Agent assemblers. The root
    /// secret remains private and the returned type cannot be serialized.
    pub fn issuer_config(
        &self,
        binary_path: String,
        authoritative_user_id: impl Into<Arc<str>>,
    ) -> GatewayMcpConfig {
        GatewayMcpConfig::from_issuer(
            self.http_addr.port(),
            self.issuer.clone(),
            binary_path,
            authoritative_user_id,
        )
    }

    pub fn stop(&mut self) {
        if let Some(handle) = self.shutdown_handle.take() {
            handle.abort();
            debug!(
                http_port = self.http_addr.port(),
                "Gateway MCP Server stop requested"
            );
        }
    }
}

impl Drop for GatewayMcpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Axum handler
// ---------------------------------------------------------------------------

async fn handle_tool_request(
    State(state): State<GatewayState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let provided_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    let claims = match body
        .get("session")
        .cloned()
        .and_then(|value| serde_json::from_value::<GatewayCapabilityClaims>(value).ok())
    {
        Some(claims)
            if claims.scope.validate().is_ok()
                && claims.session.kind == LoopbackSessionKind::Conversation
                && state
                    .issuer
                    .verify_access(GATEWAY_CAPABILITY_DOMAIN, &claims, provided_token)
                    .is_ok() => claims,
        _ => {
            warn!("Gateway MCP: invalid or unbound session authorization");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            )
                .into_response();
        }
    };

    if !claims.allows(GATEWAY_CALL_TOOL_OPERATION) {
        warn!("Gateway MCP: tools/call is outside signed capability scope");
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "forbidden"})),
        )
            .into_response();
    }

    let tool = body
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let args = body.get("args").cloned().unwrap_or(Value::Null);
    let Ok(conversation_id) = nomifun_common::ConversationId::parse(&claims.session.session_id) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "invalid session identity"}))).into_response();
    };
    let user_id = claims.user_id.clone();
    let companion_id = claims.scope.companion_id.clone();
    let ctx = CallerCtx {
        conversation_id: Some(conversation_id),
        user_id,
        companion_id,
        channel_platform: claims.scope.channel_platform.clone(),
        session_mode: claims.scope.session_mode.clone(),
        // This in-process server is the INWARD path (bundled agents on loopback);
        // never the external Remote surface.
        ..Default::default()
    };

    let deps = match state.deps.read().await.clone() {
        Some(d) => d,
        None => {
            warn!(tool, "Gateway MCP: deps not available");
            return finish(json!({"error": "service_unavailable"}));
        }
    };

    let is_instance_owner = claims.user_id.as_str() == deps.authoritative_user_id.as_ref();
    if claims.scope.instance_owner != is_instance_owner {
        warn!(user_id = %claims.user_id, "Gateway MCP: signed owner classification disagrees with runtime authority");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let registry = Registry::global();
    if !registry.contains(&tool) {
        return finish(json!({ "error": format!("Unknown tool: {tool}") }));
    }
    let domains = GatewayMcpConfig::domains_for_profile(&claims.scope.profile);
    if claims.scope.excludes(&tool)
        || !registry.tool_visible_for_caller(ctx.surface(), domains, is_instance_owner, &tool)
    {
        return finish(json!({
            "error": "session_capability_denied",
            "tool": tool,
            "profile": claims.scope.profile,
        }));
    }

    info!(tool, caller = ?ctx.conversation_id, "Gateway MCP: dispatching tool");

    // The capability registry is the single authority: it owns every tool,
    // generates its schema, and enforces the danger-tier × surface permission
    // gate. An unknown name returns a structured error the agent can recover from.
    let response_body = match registry
        .dispatch_opt(deps.clone(), ctx.clone(), &tool, &args)
        .await
    {
        Some(v) => v,
        None => {
            warn!(tool, "Gateway MCP: unknown tool");
            json!({ "error": format!("Unknown tool: {tool}") })
        }
    };

    finish(response_body)
}

async fn handle_capability_renew(
    State(state): State<GatewayState>,
    Json(request): Json<LoopbackCapabilityRenewalRequest>,
) -> impl IntoResponse {
    match state
        .issuer
        .renew::<GatewayCapabilityScope>(GATEWAY_CAPABILITY_DOMAIN, &request)
    {
        Ok(access)
            if access.claims.scope.validate().is_ok()
                && access.claims.session.kind == LoopbackSessionKind::Conversation =>
        {
            (StatusCode::OK, Json(json!(access))).into_response()
        }
        Ok(_) | Err(_) => {
            warn!("Gateway MCP: invalid capability renewal");
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            )
                .into_response()
        }
    }
}

async fn handle_capability_revoke(
    State(state): State<GatewayState>,
    Json(request): Json<LoopbackCapabilityRenewalRequest>,
) -> impl IntoResponse {
    match state
        .issuer
        .revoke(GATEWAY_CAPABILITY_DOMAIN, &request)
    {
        Ok(()) => (StatusCode::NO_CONTENT, Json(Value::Null)).into_response(),
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response(),
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
// Shared helpers for the capability handlers
// ---------------------------------------------------------------------------

/// Every conversation-domain tool needs the calling user's identity to scope
/// data access; refuse to operate without one.
pub(crate) fn require_user(ctx: &CallerCtx) -> Result<&str, Value> {
    Ok(ctx.user_id.as_str())
}

/// Wrap a serializable payload as a successful tool result.
pub(crate) fn ok<T: serde::Serialize>(payload: T) -> Value {
    match serde_json::to_value(payload) {
        Ok(v) => json!({"result": v}),
        Err(e) => json!({"error": format!("failed to serialize result: {e}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::UserId;

    const TEST_OWNER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const TEST_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const OTHER_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";

    fn child(
        server: &GatewayMcpServer,
        user_id: &str,
        conversation_id: &str,
    ) -> nomifun_api_types::GatewayMcpChildConfig {
        server
            .issuer_config("/bin/nomicore".into(), TEST_OWNER_ID)
            .issue_for_conversation(user_id, conversation_id, None, None, None, &[])
            .unwrap()
    }

    async fn post_tool(port: u16, token: Option<&str>, body: Value) -> (u16, Value) {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut req = client
            .post(format!("http://127.0.0.1:{port}/tool"))
            .json(&body);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let json: Value = resp.json().await.unwrap_or(Value::Null);
        (status, json)
    }

    async fn post_capability(
        port: u16,
        path: &str,
        request: &LoopbackCapabilityRenewalRequest,
    ) -> (u16, Value) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://127.0.0.1:{port}{path}"))
            .json(request)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn start_returns_positive_port_and_redacted_issuer() {
        let server = GatewayMcpServer::start().await.unwrap();
        assert!(server.http_port() > 0);
        let debug = format!(
            "{:?}",
            server.issuer_config("/bin/nomicore".into(), TEST_OWNER_ID)
        );
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn each_start_uses_a_fresh_issuer_secret() {
        let a = GatewayMcpServer::start().await.unwrap();
        let b = GatewayMcpServer::start().await.unwrap();
        let child_a = child(&a, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        let child_b = child(&b, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        assert_ne!(
            child_a.bootstrap.renewal.renewal_proof,
            child_b.bootstrap.renewal.renewal_proof
        );
        assert!(a
            .issuer
            .renew::<GatewayCapabilityScope>(
                GATEWAY_CAPABILITY_DOMAIN,
                &child_b.bootstrap.renewal,
            )
            .is_err());
    }

    #[tokio::test]
    async fn tool_call_requires_auth() {
        let server = GatewayMcpServer::start().await.unwrap();
        let (status, _) = post_tool(
            server.http_port(),
            None,
            json!({"tool": "nomi_list_conversations", "args": {}}),
        )
        .await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn renewal_restores_server_scope_and_revoke_ends_the_lease() {
        let server = GatewayMcpServer::start().await.unwrap();
        let child = child(&server, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        let original = &child.bootstrap.access.claims;

        let (status, body) = post_capability(
            server.http_port(),
            LOOPBACK_CAPABILITY_RENEW_PATH,
            &child.bootstrap.renewal,
        )
        .await;
        assert_eq!(status, 200);
        let renewed: nomifun_common::LoopbackCapabilityAccess<GatewayCapabilityClaims> =
            serde_json::from_value(body).unwrap();
        assert_eq!(renewed.claims.version, original.version);
        assert_eq!(renewed.claims.lease_id, original.lease_id);
        assert_eq!(renewed.claims.user_id, original.user_id);
        assert_eq!(renewed.claims.session, original.session);
        assert_eq!(renewed.claims.allowed_tools, original.allowed_tools);
        assert_eq!(renewed.claims.scope, original.scope);
        assert_ne!(renewed.claims.nonce, original.nonce);

        let (status, _) = post_capability(
            server.http_port(),
            LOOPBACK_CAPABILITY_REVOKE_PATH,
            &child.bootstrap.renewal,
        )
        .await;
        assert_eq!(status, 204);
        let (status, _) = post_capability(
            server.http_port(),
            LOOPBACK_CAPABILITY_RENEW_PATH,
            &child.bootstrap.renewal,
        )
        .await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn missing_deps_returns_unavailable() {
        // Server started but set_deps never called.
        let server = GatewayMcpServer::start().await.unwrap();
        let child = child(&server, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        let access = &child.bootstrap.access;
        let (status, body) = post_tool(
            server.http_port(),
            Some(&access.token),
            json!({"tool": "nomi_list_conversations", "args": {}, "session": access.claims}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            body.get("error").and_then(Value::as_str),
            Some("service_unavailable")
        );
    }

    #[tokio::test]
    async fn tampered_cross_conversation_and_expired_claims_are_unauthorized() {
        let server = GatewayMcpServer::start().await.unwrap();
        let child = child(&server, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        let access = &child.bootstrap.access;

        let mut forged = access.claims.clone();
        forged.session = nomifun_common::LoopbackSessionBinding::conversation(OTHER_CONVERSATION_ID);
        let (status, _) = post_tool(
            server.http_port(),
            Some(&access.token),
            json!({"tool": "nomi_list_conversations", "args": {}, "session": forged}),
        )
        .await;
        assert_eq!(status, 401);

        let now = nomifun_common::unix_time_secs();
        let expired = server
            .issuer
            .renew_at::<GatewayCapabilityScope>(
                GATEWAY_CAPABILITY_DOMAIN,
                &child.bootstrap.renewal,
                now.saturating_sub(nomifun_common::LOOPBACK_CAPABILITY_TTL_SECS + 1),
            )
            .unwrap();
        let (status, _) = post_tool(
            server.http_port(),
            Some(&expired.token),
            json!({"tool": "nomi_list_conversations", "args": {}, "session": expired.claims}),
        )
        .await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn correctly_signed_terminal_binding_is_unauthorized() {
        let server = GatewayMcpServer::start().await.unwrap();
        let child = child(&server, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        let claims = GatewayCapabilityClaims::issue(
            TEST_OWNER_ID,
            nomifun_common::LoopbackSessionBinding::terminal(
                "0190f5fe-7c00-7a00-8000-000000000001",
            ),
            [
                nomifun_api_types::GATEWAY_LIST_TOOLS_OPERATION,
                GATEWAY_CALL_TOOL_OPERATION,
            ],
            child.bootstrap.access.claims.scope.clone(),
        )
        .unwrap();
        let (token, _) = server
            .issuer
            .activate(GATEWAY_CAPABILITY_DOMAIN, &claims)
            .unwrap();
        let (status, _) = post_tool(
            server.http_port(),
            Some(&token),
            json!({"tool": "nomi_list_conversations", "args": {}, "session": claims}),
        )
        .await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn tools_call_requires_signed_operation_scope() {
        let server = GatewayMcpServer::start().await.unwrap();
        let child = child(&server, TEST_OWNER_ID, TEST_CONVERSATION_ID);
        let mut claims = child.bootstrap.access.claims.clone();
        claims.allowed_tools = vec![nomifun_api_types::GATEWAY_LIST_TOOLS_OPERATION.into()];
        let (token, _) = server
            .issuer
            .activate(GATEWAY_CAPABILITY_DOMAIN, &claims)
            .unwrap();
        let (status, body) = post_tool(
            server.http_port(),
            Some(&token),
            json!({"tool": "nomi_list_conversations", "args": {}, "session": claims}),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(body["error"], "forbidden");
    }

    #[test]
    fn require_user_returns_canonical_typed_identity() {
        let ctx = CallerCtx::default();
        assert_eq!(require_user(&ctx).unwrap(), ctx.user_id.as_str());
        let ctx = CallerCtx {
            user_id: UserId::parse("0190f5fe-7c00-7a00-8000-000000000001")
                .unwrap(),
            ..Default::default()
        };
        assert_eq!(
            require_user(&ctx).unwrap(),
            "0190f5fe-7c00-7a00-8000-000000000001"
        );
    }
}
