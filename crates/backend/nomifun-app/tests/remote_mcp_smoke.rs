//! End-to-end smoke test for the Remote capability front door (`/mcp`).
//!
//! Proves the integration P0 delivers: the MCP endpoint is mounted in the FULL
//! app router, is gated by the per-companion access token, and projects the
//! gateway Registry's Remote surface. MCP protocol correctness itself is covered
//! by rmcp's own tests; here we verify wiring + auth + the surface projection.
//!
//! The full rmcp Parts→companion_id round-trip (resolved companion_id flowing
//! through the MCP `tools/call` dispatch) is proven below by
//! `mcp_tools_call_binds_companion`, which drives a real Streamable-HTTP
//! handshake (initialize → notifications/initialized → tools/call for
//! `nomi_whoami`) and asserts the resolved companion_id appears in the JSON-RPC
//! result. The REST test additionally proves the same binding via the `/v1`
//! adapter (same dispatch + CallerCtx path).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use nomifun_gateway::{Registry, Surface};

/// `/mcp` is mounted in the full app and rejects callers without a valid
/// per-companion token; a minted token passes the gate and reaches the MCP service.
#[tokio::test]
async fn mcp_endpoint_is_mounted_and_token_gated() {
    let (app, services) = common::build_app().await;

    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "smoke", "version": "1.0" }
        }
    });
    let make_req = |token: Option<&str>| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::from(serde_json::to_vec(&init_body).unwrap())).unwrap()
    };

    let companion_id = "smoke-companion";
    let token = "smoke-companion-token";

    // No token → 401 (the front door is closed before reaching the MCP service).
    let resp = app.clone().oneshot(make_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "/mcp must reject missing token");

    // Wrong token → 401.
    let resp = app.clone().oneshot(make_req(Some("not-the-token"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "/mcp must reject a bad token");

    // Mint a per-companion token via the shared validator (same Arc the router
    // holds) and the request now passes the gate and reaches the MCP service (NOT 401).
    services
        .companion_token_validator
        .insert_token(companion_id.to_string(), nomifun_auth::token_sha256_hex(token));
    let resp = app.clone().oneshot(make_req(Some(token))).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a valid per-companion token must pass the gate (got {})",
        resp.status()
    );

    // Revocation closes it again.
    services.companion_token_validator.remove_token(companion_id);
    let resp = app.oneshot(make_req(Some(token))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "revoked token must be rejected");
}

/// LOAD-BEARING companion-binding proof over the real MCP transport: drive a
/// full Streamable-HTTP handshake through the mounted `/mcp` service
/// (initialize → notifications/initialized → tools/call for `nomi_whoami`) and
/// assert the resolved companion_id ("smoke-companion") appears in the JSON-RPC
/// result. This converts the source-verified rmcp `Parts`→`RemoteCompanion`→
/// `CallerCtx.companion_id` path into a permanent regression guard: if an
/// rmcp/transport change ever broke the `http::request::Parts` downcast in
/// `handler.rs::call_tool`, this test would catch it (the result would show a
/// null companion_id instead of "smoke-companion").
#[tokio::test]
async fn mcp_tools_call_binds_companion() {
    let (app, services) = common::build_app().await;

    let companion_id = "smoke-companion";
    let token = "smoke-companion-token";
    services
        .companion_token_validator
        .insert_token(companion_id.to_string(), nomifun_auth::token_sha256_hex(token));

    // rmcp Streamable-HTTP requires the POST Accept header to advertise BOTH
    // application/json and text/event-stream; responses come back as SSE.
    let post = |session_id: Option<&str>, body: serde_json::Value| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(sid) = session_id {
            b = b.header("mcp-session-id", sid);
        }
        b.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()
    };

    // Read the whole (terminating) SSE body and pull the JSON-RPC payload out of
    // the first non-empty `data:` line. The stream is prefixed with an SSE
    // "priming" event whose data is empty (used for client reconnection), so we
    // skip empty payloads. The transport closes the request-wise stream once the
    // response is delivered, so `to_bytes` returns.
    async fn read_sse_json(resp: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        let data = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:").map(str::trim))
            .find(|d| !d.is_empty())
            .unwrap_or_else(|| panic!("SSE body had no non-empty data: line; got: {text}"));
        serde_json::from_str(data).unwrap_or_else(|e| panic!("data line not JSON ({e}): {data}"))
    }

    // 1) initialize → captures the Mcp-Session-Id response header.
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "smoke", "version": "1.0" }
        }
    });
    let resp = app.clone().oneshot(post(None, init)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "initialize should succeed");
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("initialize must return an Mcp-Session-Id header")
        .to_string();
    // Drain the initialize SSE response so the session worker advances.
    let init_result = read_sse_json(resp).await;
    assert_eq!(init_result["id"], 1, "initialize response echoes id 1");

    // 2) notifications/initialized → 202 Accepted (no body of interest).
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let resp = app.clone().oneshot(post(Some(&session_id), initialized)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "initialized notification should be accepted");

    // 3) tools/call nomi_whoami → the result must echo the bound companion_id.
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "nomi_whoami", "arguments": {} }
    });
    let resp = app.clone().oneshot(post(Some(&session_id), call)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "tools/call should succeed");
    let rpc = read_sse_json(resp).await;
    let payload = serde_json::to_string(&rpc).unwrap();
    assert!(
        payload.contains(companion_id),
        "nomi_whoami result over /mcp must echo the bound companion_id '{companion_id}'; \
         this proves Parts→RemoteCompanion→CallerCtx.companion_id reached MCP dispatch. Got: {payload}"
    );
}

/// The REST /v1 adapter is mounted in the full app, token-gated, and serves the
/// registry-generated catalog + OpenAPI.
#[tokio::test]
async fn rest_v1_endpoint_is_mounted_and_gated() {
    let (app, services) = common::build_app().await;

    // No token → 401 on a /v1 call.
    let no_tok = Request::builder().method("GET").uri("/v1/tools").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(no_tok).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "/v1 must reject missing token");

    let companion_id = "smoke-companion";
    let token = "smoke-companion-token";
    services
        .companion_token_validator
        .insert_token(companion_id.to_string(), nomifun_auth::token_sha256_hex(token));

    let with_tok = |method: &str, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    // GET /v1/tools → 200 + non-empty catalog.
    let resp = app.clone().oneshot(with_tok("GET", "/v1/tools")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let full_count = v["tools"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(full_count > 0, "catalog must be non-empty");

    // P5: ?profile=agent → a strictly narrower curated catalog.
    let resp = app.clone().oneshot(with_tok("GET", "/v1/tools?profile=agent")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let agent_count = v["tools"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(agent_count > 0 && agent_count < full_count, "agent profile must be a non-empty strict subset");

    // GET /v1/openapi.json → 200 + an OpenAPI doc.
    let resp = app.clone().oneshot(with_tok("GET", "/v1/openapi.json")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["openapi"], "3.1.0");
    assert!(v["paths"].as_object().map(|p| !p.is_empty()).unwrap_or(false));

    // POST a read-tool with the token → 200 (passes the gate, dispatches).
    let call = Request::builder()
        .method("POST")
        .uri("/v1/tools/nomi_list_conversations")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(call).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "read tool call should succeed (got {})", resp.status());

    // LOAD-BEARING companion-binding proof: the per-companion token resolves to
    // `smoke-companion`, and that companion_id must reach dispatch. `nomi_whoami`
    // (Read cap) echoes the resolved companion_id back, so the response body must
    // contain "smoke-companion" — proving the token → companion_id → CallerCtx →
    // tool dispatch round-trip end-to-end through the mounted /v1 adapter.
    let whoami = Request::builder()
        .method("POST")
        .uri("/v1/tools/nomi_whoami")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(whoami).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "nomi_whoami call should succeed (got {})", resp.status());
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(companion_id),
        "nomi_whoami response must echo the resolved companion_id '{companion_id}' (got: {text})"
    );
}

/// The SSE streaming endpoint dispatches through the registry and terminates
/// with a `__result__` frame (verified here with a non-streaming tool; the
/// streaming path is exercised the same way, emitting deltas before it).
#[tokio::test]
async fn rest_v1_stream_endpoint_emits_result_frame() {
    let (app, services) = common::build_app().await;
    let token = "smoke-companion-token";
    services
        .companion_token_validator
        .insert_token("smoke-companion".to_string(), nomifun_auth::token_sha256_hex(token));

    let req = Request::builder()
        .method("POST")
        .uri("/v1/tools/nomi_list_conversations/stream")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(ct.starts_with("text/event-stream"), "must be an SSE stream, got {ct}");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("__result__"), "SSE stream must end with a __result__ frame; got: {text}");
}

/// The Remote surface projects a non-empty, correctly-gated subset of the
/// registry: Destructive tools are visible (Confirm) but Channel hides them,
/// and Remote is a subset of the all-permissive Desktop surface.
#[test]
fn remote_surface_projection_is_correct() {
    let remote: Vec<&str> = Registry::global().tool_specs(Surface::Remote).iter().map(|s| s.name).collect();
    let desktop: Vec<&str> = Registry::global().tool_specs(Surface::Desktop).iter().map(|s| s.name).collect();
    let channel: Vec<&str> = Registry::global().tool_specs(Surface::Channel).iter().map(|s| s.name).collect();

    assert!(!remote.is_empty(), "Remote surface must expose tools");

    // P1: the headline agent-delegation caps are exposed to external callers.
    assert!(remote.contains(&"nomi_agent_run"), "nomi_agent_run must be on the Remote surface");
    assert!(remote.contains(&"nomi_agent_result"), "nomi_agent_result must be on the Remote surface");

    // Remote ⊆ Desktop (Desktop is the most permissive surface).
    for name in &remote {
        assert!(desktop.contains(name), "Remote tool '{name}' must also be visible on Desktop");
    }

    // A Destructive tool: listed on Remote (Confirm) and Desktop, hidden on Channel (Deny).
    assert!(desktop.contains(&"nomi_delete_conversation"));
    assert!(
        remote.contains(&"nomi_delete_conversation"),
        "Destructive tools are Confirm (visible) on the Remote surface"
    );
    assert!(
        !channel.contains(&"nomi_delete_conversation"),
        "Destructive tools are hard-denied (hidden) on the Channel surface"
    );

    // Orchestration is a DESKTOP master-agent feature (the lead conversation runs
    // on a trusted desktop session). Its tools must be DESKTOP-only: visible there
    // (the lead's nomi_run_create must keep working) but ABSENT from the external
    // Remote front door (a Remote companion is never a lead, and the run reads take
    // a bare run_id with no ownership predicate). Hard-denied on Remote via
    // `deny_on(Surface::Remote)`, so they are neither advertised nor dispatchable.
    for name in ["nomi_run_create", "nomi_run_status", "nomi_run_result"] {
        assert!(
            desktop.contains(&name),
            "orchestrator tool '{name}' must remain visible on the Desktop surface (the lead)"
        );
        assert!(
            !remote.contains(&name),
            "orchestrator tool '{name}' must NOT be exposed on the external Remote surface"
        );
    }
}
