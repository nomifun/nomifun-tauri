//! MCP E2E tests beyond CRUD: connection test, agent config discovery, OAuth, auth.
//!
//! Covers test-plan sections 2 (connection test error paths), 3 (agent config discovery),
//! 4 (OAuth status), and 6 (authentication).

mod common;

use axum::http::StatusCode;
use nomifun_api_types::McpServerId;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

const CANONICAL_MCP_SERVER_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679998";

fn assert_mcp_server_id(data: &serde_json::Value) -> McpServerId {
    let raw = data["mcp_server_id"]
        .as_str()
        .expect("MCP response must contain a string mcp_server_id");
    let id = McpServerId::parse(raw).expect("mcp_server_id must be a canonical UUIDv7");
    assert_eq!(raw.len(), 36, "mcp_server_id must be a bare UUIDv7");
    assert_eq!(raw, raw.to_ascii_lowercase(), "mcp_server_id must be lowercase");
    assert!(data.get("id").is_none(), "generic id must not be exposed");
    id
}

// ===========================================================================
// CT-3: Connection test — command not found (ENOENT)
// ===========================================================================

#[tokio::test]
async fn connection_test_enoent_command() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/mcp/test-connection",
        json!({
            "name": "enoent-test",
            "transport": {
                "type": "stdio",
                "command": "nonexistent-mcp-command-xyz-12345"
            }
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let json = body_json(resp).await;
    assert!(!json["success"].as_bool().unwrap());
    assert_eq!(json["code"], "MCP_COMMAND_NOT_FOUND");
    assert_eq!(json["details"]["command"], "nonexistent-mcp-command-xyz-12345");
    assert!(!json["error"].as_str().unwrap().is_empty());
}

// ===========================================================================
// CT-4: Connection test — unreachable URL
// ===========================================================================

#[tokio::test]
async fn connection_test_unreachable_url() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/mcp/test-connection",
        json!({
            "name": "unreachable-test",
            "transport": {
                "type": "http",
                "url": "http://127.0.0.1:19999/mcp"
            }
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let json = body_json(resp).await;
    assert!(!json["success"].as_bool().unwrap());
    assert_eq!(json["code"], "MCP_CONNECTION_FAILED");
    assert_eq!(json["details"]["transport"], "http");
    assert!(!json["error"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn connection_test_accepts_saved_mcp_server_id_and_persists_failure() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let create = json_with_token(
        "POST",
        "/api/mcp/servers",
        json!({
            "name": "saved-id-test",
            "transport": {
                "type": "stdio",
                "command": "nonexistent-mcp-command-saved-id-test"
            }
        }),
        &token,
        &csrf,
    );
    let created = app.clone().oneshot(create).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_json = body_json(created).await;
    let mcp_server_id = assert_mcp_server_id(&created_json["data"]);

    let test = json_with_token(
        "POST",
        "/api/mcp/test-connection",
        json!({
            "mcp_server_id": mcp_server_id,
            "name": "saved-id-test",
            "transport": {
                "type": "stdio",
                "command": "nonexistent-mcp-command-saved-id-test"
            }
        }),
        &token,
        &csrf,
    );
    let tested = app.clone().oneshot(test).await.unwrap();
    assert_eq!(tested.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let tested_json = body_json(tested).await;
    assert_eq!(tested_json["code"], "MCP_COMMAND_NOT_FOUND");

    let listed = app
        .clone()
        .oneshot(get_with_token("/api/mcp/servers", &token))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_json = body_json(listed).await;
    let persisted = listed_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["mcp_server_id"].as_str() == Some(mcp_server_id.as_str()))
        .unwrap();
    assert_eq!(assert_mcp_server_id(persisted), mcp_server_id);
    assert_eq!(persisted["last_test_status"], "error");
}

#[tokio::test]
async fn connection_test_rejects_legacy_mcp_server_id_shapes() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let transport = json!({
        "type": "http",
        "url": "http://127.0.0.1:19999/mcp"
    });

    let invalid_requests = [
        (
            "generic id",
            json!({
                "id": 1,
                "name": "legacy-generic-id-test",
                "transport": transport.clone()
            }),
        ),
        (
            "integer mcp_server_id",
            json!({
                "mcp_server_id": 1,
                "name": "legacy-integer-id-test",
                "transport": transport.clone()
            }),
        ),
        (
            "numeric string mcp_server_id",
            json!({
                "mcp_server_id": "1",
                "name": "legacy-numeric-string-id-test",
                "transport": transport.clone()
            }),
        ),
        (
            "prefixed UUIDv7 mcp_server_id",
            json!({
                "mcp_server_id": format!("mcp_{CANONICAL_MCP_SERVER_ID}"),
                "name": "legacy-prefixed-id-test",
                "transport": transport.clone()
            }),
        ),
        (
            "UUIDv4 mcp_server_id",
            json!({
                "mcp_server_id": "550e8400-e29b-41d4-a716-446655440000",
                "name": "legacy-uuidv4-test",
                "transport": transport.clone()
            }),
        ),
        (
            "uppercase UUIDv7 mcp_server_id",
            json!({
                "mcp_server_id": CANONICAL_MCP_SERVER_ID.to_ascii_uppercase(),
                "name": "legacy-uppercase-id-test",
                "transport": transport.clone()
            }),
        ),
    ];

    for (shape, body) in invalid_requests {
        let req = json_with_token("POST", "/api/mcp/test-connection", body, &token, &csrf);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{shape} must be rejected");
    }
}

// ===========================================================================
// AS-1: Get agent configs (may return empty in test env)
// ===========================================================================

#[tokio::test]
async fn get_agent_configs() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/mcp/agent-configs", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    // In test env, data is an array (may be empty or contain nomifun adapter)
    assert!(json["data"].is_array());
}

// ===========================================================================
// OA-1: OAuth check status — unauthenticated server
// ===========================================================================

#[tokio::test]
async fn oauth_check_status_unauthenticated_server() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/mcp/oauth/check-status",
        json!({ "server_url": "https://unknown-server.example.com" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(!json["data"]["authenticated"].as_bool().unwrap());
}

// ===========================================================================
// OA-3: Get all authenticated servers (empty at start)
// ===========================================================================

#[tokio::test]
async fn oauth_authenticated_servers_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/mcp/oauth/authenticated", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
    assert_eq!(json["data"], json!([]));
}

// ===========================================================================
// OA-7: Logout from never-authenticated server (idempotent)
// ===========================================================================

#[tokio::test]
async fn oauth_logout_idempotent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/mcp/oauth/logout",
        json!({ "server_url": "https://never-authed.example.com" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["success"].as_bool().unwrap());
}

// ===========================================================================
// AU-1: Unauthenticated access to various MCP endpoints
// ===========================================================================

#[tokio::test]
async fn unauthenticated_get_servers_rejected() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/mcp/servers")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    // CSRF middleware rejects before auth can run
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unauthenticated_post_server_rejected() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/mcp/servers")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({
                "name": "test",
                "transport": { "type": "stdio", "command": "npx" }
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// AU-3: Valid token accesses MCP routes successfully
#[tokio::test]
async fn authenticated_access_succeeds() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/mcp/servers", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// AU-2: Invalid Bearer token is rejected by auth middleware (403 per API spec)
#[tokio::test]
async fn invalid_token_rejected() {
    let (app, _services) = build_app().await;

    // GET bypasses CSRF → auth middleware sees invalid Bearer → 403
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/mcp/servers")
        .header("authorization", "Bearer invalid-jwt-token-abc123")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
