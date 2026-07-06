//! In-process HTTP half of the Desktop Gateway MCP.
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
use nomifun_common::generate_id;
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
    auth_token: String,
    deps: DepsSlot,
}

/// In-process HTTP MCP server for the desktop gateway tools.
pub struct GatewayMcpServer {
    http_addr: SocketAddr,
    auth_token: String,
    shutdown_handle: Option<tokio::task::JoinHandle<()>>,
    deps_slot: DepsSlot,
}

impl GatewayMcpServer {
    /// Bind a fresh `127.0.0.1:0` listener, mint a random bearer token, and
    /// start serving `POST /tool`. Deps must be wired separately via
    /// [`set_deps`](Self::set_deps) before the first tool call arrives.
    pub async fn start() -> Result<Self, String> {
        let auth_token = generate_id();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind gateway MCP HTTP listener: {e}"))?;
        let http_addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to read gateway MCP local addr: {e}"))?;

        let deps_slot: DepsSlot = Arc::new(RwLock::new(None));

        let state = GatewayState {
            auth_token: auth_token.clone(),
            deps: deps_slot.clone(),
        };

        let app = axum::Router::new()
            .route("/tool", axum::routing::post(handle_tool_request))
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
            auth_token,
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

    pub fn auth_token(&self) -> &str {
        &self.auth_token
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

    if provided_token != state.auth_token {
        warn!("Gateway MCP: unauthorized request");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let tool = body
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let args = body.get("args").cloned().unwrap_or(Value::Null);
    let ctx = CallerCtx {
        conversation_id: body
            .get("conversation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        user_id: body
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        // Optional: only master-agent / companion sessions with a companion binding
        // carry it; empty is normalized to None.
        companion_id: body
            .get("companion_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        // Optional: only channel master-agent sessions carry it.
        channel_platform: body
            .get("channel_platform")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        session_mode: body
            .get("session_mode")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
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

    info!(tool, caller = %ctx.conversation_id, "Gateway MCP: dispatching tool");

    // The capability registry is the single authority: it owns every tool,
    // generates its schema, and enforces the danger-tier × surface permission
    // gate. An unknown name returns a structured error the agent can recover from.
    let response_body = match Registry::global()
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
    if ctx.user_id.is_empty() {
        Err(json!({"error": "missing caller user identity (NOMI_GW_MCP_USER_ID)"}))
    } else {
        Ok(&ctx.user_id)
    }
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

    #[tokio::test]
    async fn start_returns_positive_port_and_token() {
        let server = GatewayMcpServer::start().await.unwrap();
        assert!(server.http_port() > 0);
        assert!(!server.auth_token().is_empty());
    }

    #[tokio::test]
    async fn each_start_uses_a_fresh_auth_token() {
        let a = GatewayMcpServer::start().await.unwrap();
        let b = GatewayMcpServer::start().await.unwrap();
        assert_ne!(a.auth_token(), b.auth_token());
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
    async fn missing_deps_returns_unavailable() {
        // Server started but set_deps never called.
        let server = GatewayMcpServer::start().await.unwrap();
        let (status, body) = post_tool(
            server.http_port(),
            Some(server.auth_token()),
            json!({"tool": "nomi_list_conversations", "args": {}}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            body.get("error").and_then(Value::as_str),
            Some("service_unavailable")
        );
    }

    #[test]
    fn require_user_rejects_empty_identity() {
        let ctx = CallerCtx::default();
        assert!(require_user(&ctx).is_err());
        let ctx = CallerCtx {
            user_id: "u1".into(),
            ..Default::default()
        };
        assert_eq!(require_user(&ctx).unwrap(), "u1");
    }
}
