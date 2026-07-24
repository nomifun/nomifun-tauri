//! E2E tests for Remote Agent CRUD, connection test, and handshake endpoints.

mod common;

use axum::http::StatusCode;
use nomifun_common::RemoteAgentId;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_with_token, json_with_token, setup_and_login};

const MISSING_REMOTE_AGENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679996";

// ── Helpers ───────────────────────────────────────────────────────────

fn bearer_agent_body() -> serde_json::Value {
    json!({
        "name": "Test Remote Server",
        "protocol": "openclaw",
        "url": "wss://remote.example.com",
        "auth_type": "bearer",
        "auth_token": "my-secret-token-1234",
        "description": "Production agent"
    })
}

fn openclaw_agent_body() -> serde_json::Value {
    json!({
        "name": "OpenClaw Agent",
        "protocol": "openclaw",
        "url": "wss://openclaw.example.com",
        "auth_type": "none"
    })
}

async fn create_agent(app: &mut axum::Router, token: &str, csrf: &str, body: serde_json::Value) -> serde_json::Value {
    let req = json_with_token("POST", "/api/remote-agents", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    wire_remote_agent_id(&json["data"]);
    json
}

fn wire_remote_agent_id(data: &serde_json::Value) -> &str {
    assert!(
        data.get("id").is_none(),
        "remote-agent wire responses must reject the legacy id field"
    );
    data["remote_agent_id"]
        .as_str()
        .expect("remote_agent_id must be a string")
}

// ── 1.1 Create Remote Agent ─────────────────────────────────────────

#[tokio::test]
async fn t1_1_create_bearer_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let json = create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;

    let data = &json["data"];
    let remote_agent_id = wire_remote_agent_id(data);
    RemoteAgentId::parse(remote_agent_id)
        .expect("remote_agent_id must be a canonical UUIDv7");
    assert_eq!(data["name"], "Test Remote Server");
    assert_eq!(data["protocol"], "openclaw");
    assert_eq!(data["url"], "wss://remote.example.com");
    assert_eq!(data["auth_type"], "bearer");
    // Auth token should be masked
    assert_eq!(data["auth_token"], "***1234");
    assert_eq!(data["status"], "unknown");
    assert_eq!(data["description"], "Production agent");
    assert!(data["created_at"].as_i64().is_some());
    assert!(data["updated_at"].as_i64().is_some());
}

#[tokio::test]
async fn t1_2_create_openclaw_agent_generates_device_keys() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let json = create_agent(&mut app, &token, &csrf, openclaw_agent_body()).await;

    let data = &json["data"];
    assert_eq!(data["protocol"], "openclaw");
    // Device ID and public key should be generated
    assert_eq!(data["device_id"].as_str().unwrap().len(), 64);
    assert!(data["device_public_key"].as_str().is_some());
    // Private key should NOT be in the response
    assert!(data.get("device_private_key").is_none());
}

#[tokio::test]
async fn t1_3_create_missing_required_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({ "name": "test" });
    let req = json_with_token("POST", "/api/remote-agents", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t1_4_create_unauthenticated() {
    let (app, _services) = build_app().await;

    let body = bearer_agent_body();
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/remote-agents")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── 1.2 List Remote Agents ──────────────────────────────────────────

#[tokio::test]
async fn t2_1_list_returns_agents_without_auth_token() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;

    let req = get_with_token("/api/remote-agents", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = json["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);

    // auth_token should NOT appear in list response
    assert!(data[0].get("auth_token").is_none());
    wire_remote_agent_id(&data[0]);
    assert_eq!(data[0]["name"], "Test Remote Server");
}

#[tokio::test]
async fn t2_2_list_empty() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/remote-agents", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = json["data"].as_array().unwrap();
    assert!(data.is_empty());
}

// ── 1.3 Get Single Remote Agent ─────────────────────────────────────

#[tokio::test]
async fn t3_1_get_single_agent_with_masked_token() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;
    let remote_agent_id = wire_remote_agent_id(&created["data"]).to_owned();

    let req = get_with_token(
        &format!("/api/remote-agents/{remote_agent_id}"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = &json["data"];
    assert_eq!(wire_remote_agent_id(data), remote_agent_id);
    assert_eq!(data["auth_token"], "***1234");
    assert_eq!(data["description"], "Production agent");
}

#[tokio::test]
async fn t3_2_get_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token(
        &format!("/api/remote-agents/{MISSING_REMOTE_AGENT_ID}"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 1.4 Update Remote Agent ────────────────────────────────────────

#[tokio::test]
async fn t4_1_update_name_only() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;
    let remote_agent_id = wire_remote_agent_id(&created["data"]).to_owned();

    let body = json!({ "name": "Updated Name" });
    let req = json_with_token(
        "PUT",
        &format!("/api/remote-agents/{remote_agent_id}"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(wire_remote_agent_id(&json["data"]), remote_agent_id);
    assert_eq!(json["data"]["name"], "Updated Name");
    // Other fields preserved
    assert_eq!(json["data"]["protocol"], "openclaw");
    assert_eq!(json["data"]["url"], "wss://remote.example.com");
}

#[tokio::test]
async fn t4_2_update_multiple_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;
    let remote_agent_id = wire_remote_agent_id(&created["data"]).to_owned();

    let body = json!({
        "name": "Updated",
        "url": "wss://new-url.example.com",
        "auth_token": "new-super-secret-token"
    });
    let req = json_with_token(
        "PUT",
        &format!("/api/remote-agents/{remote_agent_id}"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(wire_remote_agent_id(&json["data"]), remote_agent_id);
    assert_eq!(json["data"]["name"], "Updated");
    assert_eq!(json["data"]["url"], "wss://new-url.example.com");
    assert_eq!(json["data"]["auth_token"], "***oken");
}

#[tokio::test]
async fn t4_3_update_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({ "name": "Doesn't Matter" });
    let req = json_with_token(
        "PUT",
        &format!("/api/remote-agents/{MISSING_REMOTE_AGENT_ID}"),
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 1.5 Delete Remote Agent ────────────────────────────────────────

#[tokio::test]
async fn t5_1_delete_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let created = create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;
    let remote_agent_id = wire_remote_agent_id(&created["data"]).to_owned();

    let req = delete_with_token(
        &format!("/api/remote-agents/{remote_agent_id}"),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify it's gone
    let req = get_with_token(
        &format!("/api/remote-agents/{remote_agent_id}"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn t5_2_delete_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token(
        &format!("/api/remote-agents/{MISSING_REMOTE_AGENT_ID}"),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 1.6 Connection Test ─────────────────────────────────────────────

#[tokio::test]
async fn t6_1_test_connection_invalid_protocol() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "url": "http://example.com",
        "auth_type": "bearer"
    });
    let req = json_with_token("POST", "/api/remote-agents/test-connection", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

#[tokio::test]
async fn t6_2_test_connection_unauthenticated() {
    let (app, _services) = build_app().await;

    let body = json!({
        "url": "wss://remote.example.com"
    });
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/remote-agents/test-connection")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── 1.7 Protocol validation and handshake ────────────────────────────

#[tokio::test]
async fn t7_1_create_rejects_non_openclaw_protocol() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "Unsupported ACP Remote",
        "protocol": "acp",
        "url": "wss://remote.example.com",
        "auth_type": "none"
    });
    let req = json_with_token(
        "POST",
        "/api/remote-agents",
        body,
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn t7_2_handshake_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        &format!("/api/remote-agents/{MISSING_REMOTE_AGENT_ID}/handshake"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Full CRUD lifecycle ─────────────────────────────────────────────

#[tokio::test]
async fn t8_full_crud_lifecycle() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create
    let created = create_agent(&mut app, &token, &csrf, bearer_agent_body()).await;
    let remote_agent_id = wire_remote_agent_id(&created["data"]).to_owned();

    // Read list
    let req = get_with_token("/api/remote-agents", &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let listed = json["data"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(wire_remote_agent_id(&listed[0]), remote_agent_id);

    // Read single
    let req = get_with_token(
        &format!("/api/remote-agents/{remote_agent_id}"),
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(wire_remote_agent_id(&json["data"]), remote_agent_id);

    // Update
    let body = json!({ "name": "Renamed Server", "description": "Updated desc" });
    let req = json_with_token(
        "PUT",
        &format!("/api/remote-agents/{remote_agent_id}"),
        body,
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(wire_remote_agent_id(&json["data"]), remote_agent_id);
    assert_eq!(json["data"]["name"], "Renamed Server");
    assert_eq!(json["data"]["description"], "Updated desc");

    // Delete
    let req = delete_with_token(
        &format!("/api/remote-agents/{remote_agent_id}"),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify deleted
    let req = get_with_token("/api/remote-agents", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}
