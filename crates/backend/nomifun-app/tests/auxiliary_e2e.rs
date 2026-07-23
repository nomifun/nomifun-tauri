//! E2E integration tests for auxiliary conversation routes.
//!
//! Tests cover: workspace browse, side-question,
//! slash-commands, and openclaw-runtime endpoints.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

const MISSING_CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679998";

// ── Helpers ─────────────────────────────────────────────────────

fn create_conv_body(name: &str, agent_type: &str) -> serde_json::Value {
    json!({
        "type": agent_type,
        "name": name,
        "extra": { "workspace": "/project" }
    })
}

fn create_conv_body_with_workspace(name: &str, agent_type: &str, workspace: &str) -> serde_json::Value {
    json!({
        "type": agent_type,
        "name": name,
        "extra": { "workspace": workspace }
    })
}

async fn create_conversation_with_workspace(
    app: &mut axum::Router,
    token: &str,
    csrf: &str,
    name: &str,
    agent_type: &str,
    workspace: &str,
) -> String {
    let req = common::json_with_token(
        "POST",
        "/api/conversations",
        create_conv_body_with_workspace(name, agent_type, workspace),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["conversation_id"].as_str().unwrap().to_owned()
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str, agent_type: &str) -> String {
    let req = common::json_with_token(
        "POST",
        "/api/conversations",
        create_conv_body(name, agent_type),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = common::body_json(resp).await;
    json["data"]["conversation_id"].as_str().unwrap().to_owned()
}

async fn build_app() -> (axum::Router, nomifun_app::AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = nomifun_app::AppServices::from_config(db, &nomifun_app::AppConfig::default())
        .await
        .unwrap();
    let router = nomifun_app::create_router(&services).await;
    (router, services)
}

async fn setup_owner(
    app: &mut axum::Router,
    services: &nomifun_app::AppServices,
) -> (String, String) {
    setup_and_login(app, services, "admin", "StrongP@ss1").await
}

// ── 9.1 Workspace browse ────────────────────────────────────────

#[tokio::test]
async fn workspace_browse_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/conversations/test-conv/workspace?path=/src")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn workspace_browse_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;

    // Seed a real workspace on disk so the handler can canonicalize it.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), b"// hi").unwrap();

    let ws = tmp.path().to_string_lossy().into_owned();
    let conv_id = create_conversation_with_workspace(&mut app, &token, &csrf, "Test Conv", "acp", &ws).await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/workspace?path=/src"), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    // Workspace comes from DB; no active agent required.
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "lib.rs");
    assert_eq!(entries[0]["type"], "file");
}

#[tokio::test]
async fn workspace_browse_conversation_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token(
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/workspace?path=/src"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_browse_empty_path() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    let req = get_with_token("/api/conversations/some-conv/workspace?path=", &token);
    let resp = app.oneshot(req).await.unwrap();
    // Empty path should return 400 (validated before agent lookup)
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_browse_treats_symlinked_skill_dir_as_directory() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let builtin = tmp.path().join("builtin-skills/auto-inject/nomifun-skills");
    std::fs::create_dir_all(workspace.join(".claude/skills")).unwrap();
    std::fs::create_dir_all(&builtin).unwrap();
    std::fs::write(builtin.join("SKILL.md"), b"---\ndescription: test\n---\nbody").unwrap();
    std::os::unix::fs::symlink(&builtin, workspace.join(".claude/skills/nomifun-skills")).unwrap();

    let ws = workspace.to_string_lossy().into_owned();
    let conv_id = create_conversation_with_workspace(&mut app, &token, &csrf, "Test Conv", "acp", &ws).await;

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/workspace?path=/.claude/skills"),
        &token,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries
            .iter()
            .any(|entry| entry["name"] == "nomifun-skills" && entry["type"] == "directory"),
        "symlinked skill dir should stay visible as directory: {entries:?}"
    );

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/workspace?path=/.claude/skills/nomifun-skills"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["name"] == "SKILL.md" && entry["type"] == "file"),
        "symlinked skill dir should remain browsable: {entries:?}"
    );
}

// ── 9.1b Terminal workspace browse ──────────────────────────────
//
// The terminal analogue of `GET /api/conversations/{id}/workspace`:
// `GET /api/terminals/{id}/workspace` lists one directory level under the
// session's server-authoritative `cwd`. These e2e tests guard the routing +
// auth wiring (the service layer has its own unit tests); they mirror
// `workspace_browse_requires_auth` / `workspace_browse_no_active_task` above.

/// Create a terminal session row WITHOUT a live PTY. `defer_spawn: true` makes
/// the service persist the row and defer the PTY to the first resize, so no
/// process is spawned in the test harness. Returns the DB-minted id as a string.
async fn create_terminal_with_cwd(app: &mut axum::Router, token: &str, csrf: &str, cwd: &str) -> String {
    let req = common::json_with_token(
        "POST",
        "/api/terminals",
        json!({
            "name": "Test Terminal",
            "cwd": cwd,
            "command": "cat",
            "defer_spawn": true
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "terminal create should succeed");
    let json = common::body_json(resp).await;
    json["data"]["terminal_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn terminal_workspace_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/terminals/0190f5fe-7c00-7a00-8abc-012345678901/workspace?path=")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    // Mounted behind the auth middleware → unauthenticated request rejected
    // before the handler runs (same status as the conversation analogue).
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn terminal_workspace_lists_cwd_entries() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;

    // Seed a real workspace on disk; the handler derives the root from the
    // session's `cwd` (server-authoritative) and lists one level.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let term_id = create_terminal_with_cwd(&mut app, &token, &csrf, &cwd).await;

    let req = get_with_token(&format!("/api/terminals/{term_id}/workspace?path="), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    // Root comes from the DB row's cwd; no live PTY required.
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let entries = json["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "hello.txt");
    assert_eq!(entries[0]["type"], "file");
}

#[tokio::test]
async fn terminal_workspace_not_found() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_owner(&mut app, &services).await;

    // Authenticated, but no such terminal session row → 404 (the service
    // surfaces a missing row as NotFound before any filesystem access).
    let req = get_with_token("/api/terminals/0190f5fe-7c00-7a00-8abc-012345679999/workspace?path=", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 9.2 Side question ───────────────────────────────────────────

#[tokio::test]
async fn side_question_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/conversations/test-conv/side-question")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"question":"test?"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn side_question_empty_question() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "user1", "pass123").await;

    // side-question is now dispatched to AgentRuntimeHandle after the
    // conversation lookup, so a missing conversation surfaces as 404
    // before the empty-question check gets a chance to fire.
    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{MISSING_CONVERSATION_ID}/side-question"),
        json!({ "question": "" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn side_question_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Side Q Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/side-question"),
        json!({ "question": "What is this?" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // No active agent → 404
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 9.4 Slash commands ──────────────────────────────────────────

#[tokio::test]
async fn slash_commands_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/conversations/test-conv/slash-commands")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn slash_commands_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Slash Test", "acp").await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/slash-commands"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 9.5 OpenClaw runtime ────────────────────────────────────────

#[tokio::test]
async fn openclaw_runtime_requires_auth() {
    let (app, _) = build_app().await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/conversations/test-conv/openclaw/runtime")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn openclaw_runtime_no_active_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "OpenClaw Test", "openclaw-gateway").await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/openclaw/runtime"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Confirmation routes (no active runtime → graceful defaults) ─────

#[tokio::test]
async fn list_confirmations_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test", "acp").await;

    let req = get_with_token(&format!("/api/conversations/{conv_id}/confirmations"), &token);
    let resp = app.oneshot(req).await.unwrap();
    // No active agent → returns empty list gracefully
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn confirm_call_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Confirm Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/confirmations/call-1/confirm"),
        json!({ "msg_id": "msg-1", "data": { "value": "allow" }, "always_allow": false }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn check_approval_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Approval Test", "acp").await;

    let req = get_with_token(
        &format!("/api/conversations/{conv_id}/approvals/check?action=edit_file"),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    // No active agent → returns approved=false gracefully
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["approved"], false);
}

// ── Stop + Warmup (no active runtime → idempotent success) ───────

#[tokio::test]
async fn stop_stream_no_task() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_owner(&mut app, &services).await;
    let conv_id = create_conversation(&mut app, &token, &csrf, "Stop Test", "acp").await;

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conv_id}/cancel"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // Stop with no active agent is idempotent.
    assert_eq!(resp.status(), StatusCode::OK);
}
