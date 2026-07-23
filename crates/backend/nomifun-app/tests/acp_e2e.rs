//! E2E integration tests for ACP management routes.
//!
//! Tests cover: agents list, agents/refresh, agents/test, health-check,
//! and session-bound routes (mode/model).

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

const MISSING_CONVERSATION_ID: &str =
    "0190f5fe-7c00-7a00-8000-000000000099";

// ── Global ACP routes ────────────────────────────────────────────

#[tokio::test]
async fn list_agents_returns_array() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/agents", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert!(body["data"].is_array());
    let agents = body["data"].as_array().unwrap();
    assert!(agents.iter().any(|a| a["agent_type"] == "nomi"));
    assert!(
        agents
            .iter()
            .all(|agent| agent["agent_id"].is_string() && agent.get("id").is_none()),
        "AgentMetadata wire rows must expose agent_id and reject legacy id"
    );
}

#[tokio::test]
async fn refresh_agents_returns_array() {
    let (mut app, services) = build_app().await;
    let (token, csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/agents/refresh", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert!(body["data"].is_array());
}

#[tokio::test]
async fn test_custom_agent_nonexistent_command() {
    let (mut app, services) = build_app().await;
    let (token, csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Endpoint was renamed from /api/agents/test to /api/agents/custom/try-connect
    // when the custom-agent CRUD routes were introduced.  The new endpoint always
    // returns HTTP 200 and encodes failure in the JSON body (step = "fail_cli" or
    // "fail_acp"), so we assert on the body rather than the HTTP status.
    let req = json_with_token(
        "POST",
        "/api/agents/custom/try-connect",
        json!({ "command": "/nonexistent/path/to/agent" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = common::body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["step"], "fail_cli");
}

#[tokio::test]
async fn health_check_returns_status() {
    let (mut app, services) = build_app().await;
    let (token, csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/agents/health-check",
        json!({ "backend": "claude" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    // available is a boolean
    assert!(body["data"]["available"].is_boolean());
    // latency should be present
    assert!(body["data"]["latency"].is_number());
}

#[tokio::test]
async fn health_check_unknown_backend_reports_unavailable() {
    let (mut app, services) = build_app().await;
    let (token, csrf) =
        setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Same rationale as `detect_cli_unknown_backend_returns_null_path`:
    // unknown backends are valid at the request layer and surface as
    // `available: false` with an error string.
    let req = json_with_token(
        "POST",
        "/api/agents/health-check",
        json!({ "backend": "iFlow" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["available"], false);
}

// ── Session-bound ACP routes (no active runtime → 404) ──────────────

#[tokio::test]
async fn get_mode_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token(
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/mode"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_mode_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = json_with_token(
        "PUT",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/mode"),
        json!({ "mode": "code" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_model_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token(
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/model"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_model_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = json_with_token(
        "PUT",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/model"),
        json!({ "model_id": "claude-sonnet-4" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
