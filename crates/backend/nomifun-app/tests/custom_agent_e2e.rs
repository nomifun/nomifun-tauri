//! E2E integration tests for Custom Agent CRUD and try-connect endpoints.
//!
//! Covers:
//!   - Create / update / delete / toggle-enable happy paths
//!   - Empty-field validation (name, command)
//!   - NotFound on missing id
//!   - Forbidden when operating on a builtin id through the custom-only paths
//!   - Test-on-save CLI-not-found rejection (runs the real probe)
//!
//! Gates the probe with NOMIFUN_BYPASS_PROBE for happy paths so CI does not
//! need an ACP CLI installed. Unset bypass for the single test that
//! verifies the CLI-not-found path.
//!
//! Thread safety: `NOMIFUN_BYPASS_PROBE` is a process-wide env var.  Tests
//! that set or clear it hold `ENV_MUTEX` for their entire body so no two
//! tests race on the var simultaneously.

mod common;

use std::sync::OnceLock;

use tokio::sync::Mutex;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

// ── Global lock for env-var mutation ─────────────────────────────────────────
//
// Any test that reads or writes NOMIFUN_BYPASS_PROBE must hold this lock for
// its entire body.  This serialises all probe-sensitive tests within a single
// test binary regardless of how many threads `cargo test` uses.

static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn env_mutex() -> &'static Mutex<()> {
    ENV_MUTEX.get_or_init(|| Mutex::new(()))
}

#[cfg(windows)]
fn available_test_command() -> &'static str {
    "cmd.exe"
}

#[cfg(not(windows))]
fn available_test_command() -> &'static str {
    "sh"
}

/// Acquire ENV_MUTEX.  `tokio::sync::Mutex` guards are async-aware and
/// may be held across await points without triggering clippy's
/// `await_holding_lock` lint.
async fn lock_env() -> tokio::sync::MutexGuard<'static, ()> {
    env_mutex().lock().await
}

// ── Helper: create a custom agent and return (status, body) ──────────────────

async fn create_agent(app: &mut axum::Router, token: &str, csrf: &str, body: Value) -> (StatusCode, Value) {
    let req = json_with_token("POST", "/api/agents/custom", body, token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let json = body_json(resp).await;
    (status, json)
}

async fn list_agents(app: &mut axum::Router, token: &str) -> Value {
    let req = get_with_token("/api/agents", token);
    let resp = app.clone().oneshot(req).await.unwrap();
    body_json(resp).await
}

// ── Happy path: create → list → update → toggle → delete ─────────────────────

#[tokio::test]
async fn custom_agent_full_roundtrip() {
    let _guard = lock_env().await;
    // SAFETY: single env-var mutation under ENV_MUTEX; restored at function end.
    unsafe {
        std::env::set_var("NOMIFUN_BYPASS_PROBE", "1");
    }

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Use a platform-native command so which::which resolves it and the registry marks
    // the new row as available (list_all filters out unavailable rows).
    let (status, json) = create_agent(
        &mut app,
        &token,
        &csrf,
        json!({
            "name": "My Claude",
            "command": available_test_command(),
            "icon": "🤖",
            "args": ["--acp"],
            "env": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["success"], true);
    let id = json["data"]["id"].as_str().expect("id in response").to_owned();
    assert_eq!(json["data"]["name"], "My Claude");
    assert_eq!(json["data"]["agent_source"], "custom");
    assert_eq!(json["data"]["icon"], "🤖");

    // List — agent should be visible
    let listed = list_agents(&mut app, &token).await;
    let agents = listed["data"].as_array().expect("array");
    assert!(
        agents.iter().any(|a| a["id"] == id),
        "newly created agent should appear in GET /api/agents"
    );

    // Keep the platform-native command so the row stays available after rehydrate.
    let req = json_with_token(
        "PUT",
        &format!("/api/agents/custom/{id}"),
        json!({
            "name": "My Claude v2",
            "command": available_test_command(),
            "icon": "🚀",
            "args": [],
            "env": []
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], id, "id must survive update");
    assert_eq!(json["data"]["name"], "My Claude v2");
    assert_eq!(json["data"]["icon"], "🚀");

    // Toggle disabled
    let req = json_with_token(
        "PATCH",
        &format!("/api/agents/{id}/enabled"),
        json!({ "enabled": false }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], false);

    // Re-enable
    let req = json_with_token(
        "PATCH",
        &format!("/api/agents/{id}/enabled"),
        json!({ "enabled": true }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete
    let req = json_with_token(
        "DELETE",
        &format!("/api/agents/custom/{id}"),
        json!(null),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["deleted"], true);

    // Post-delete list must not contain the id
    let listed = list_agents(&mut app, &token).await;
    let agents = listed["data"].as_array().unwrap();
    assert!(
        agents.iter().all(|a| a["id"] != id),
        "deleted agent should disappear from GET /api/agents"
    );

    unsafe {
        std::env::remove_var("NOMIFUN_BYPASS_PROBE");
    }
}

// ── Advanced overrides flow ───────────────────────────────────────────────────

#[tokio::test]
async fn custom_agent_advanced_overrides_persist() {
    let _guard = lock_env().await;
    unsafe {
        std::env::set_var("NOMIFUN_BYPASS_PROBE", "1");
    }

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let (status, json) = create_agent(
        &mut app,
        &token,
        &csrf,
        json!({
            "name": "With Advanced",
            "command": available_test_command(),
            "advanced": {
                "yolo_id": "bypassPermissions",
                "native_skills_dirs": [".claude/skills"],
                "description": "test",
                "unknown_ignored_key": 42
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["yolo_id"], "bypassPermissions");
    assert_eq!(json["data"]["native_skills_dirs"], json!([".claude/skills"]));
    assert_eq!(json["data"]["description"], "test");

    unsafe {
        std::env::remove_var("NOMIFUN_BYPASS_PROBE");
    }
}

// ── Bad path: validation ──────────────────────────────────────────────────────
//
// These tests exercise the validate_upsert() path which fires before the
// probe, so they do not need NOMIFUN_BYPASS_PROBE and do not need the lock.

#[tokio::test]
async fn create_rejects_empty_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let (status, json) = create_agent(
        &mut app,
        &token,
        &csrf,
        json!({ "name": "", "command": available_test_command() }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["success"], false);
    assert!(json["error"].as_str().unwrap().to_lowercase().contains("name"));
}

#[tokio::test]
async fn create_rejects_empty_command() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let (status, json) = create_agent(&mut app, &token, &csrf, json!({ "name": "x", "command": "   " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().to_lowercase().contains("command"));
}

// ── Bad path: 404 and 403 ─────────────────────────────────────────────────────

#[tokio::test]
async fn update_unknown_id_returns_404() {
    let _guard = lock_env().await;
    unsafe {
        std::env::set_var("NOMIFUN_BYPASS_PROBE", "1");
    }

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PUT",
        "/api/agents/custom/does-not-exist",
        json!({ "name": "x", "command": available_test_command() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    unsafe {
        std::env::remove_var("NOMIFUN_BYPASS_PROBE");
    }
}

#[tokio::test]
async fn update_builtin_id_returns_403() {
    let _guard = lock_env().await;
    unsafe {
        std::env::set_var("NOMIFUN_BYPASS_PROBE", "1");
    }

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // agent_builtin_claude is the seeded Claude id (builtin) from the baseline schema.
    let req = json_with_token(
        "PUT",
        "/api/agents/custom/agent_builtin_claude",
        json!({ "name": "hacked", "command": available_test_command() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    unsafe {
        std::env::remove_var("NOMIFUN_BYPASS_PROBE");
    }
}

#[tokio::test]
async fn delete_builtin_id_returns_403() {
    // delete_custom_agent checks agent_source before calling the probe,
    // so no bypass needed here.
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "DELETE",
        "/api/agents/custom/agent_builtin_claude",
        json!(null),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn set_enabled_unknown_id_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PATCH",
        "/api/agents/missing-id/enabled",
        json!({ "enabled": false }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Test-on-save: CLI not found ───────────────────────────────────────────────

#[tokio::test]
async fn test_on_save_cli_not_found_blocks_upsert() {
    // Hold ENV_MUTEX to guarantee NOMIFUN_BYPASS_PROBE is unset for the
    // duration of this test — no bypass-setting test can interleave.
    let _guard = lock_env().await;
    unsafe {
        // Ensure clean state regardless of test ordering.
        std::env::remove_var("NOMIFUN_BYPASS_PROBE");
    }

    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let (status, json) = create_agent(
        &mut app,
        &token,
        &csrf,
        json!({
            "name": "bad",
            "command": "nomifun-definitely-nonexistent-xyz"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = json["error"].as_str().expect("error string");
    // AppError::BadRequest is serialized as "Bad request: <msg>", so we
    // check that the marker string appears anywhere in the error field.
    assert!(
        err.contains("cli_not_found:"),
        "error must carry cli_not_found: marker, got: {err}"
    );

    // DB must not have the row.
    let listed = list_agents(&mut app, &token).await;
    let agents = listed["data"].as_array().unwrap();
    assert!(
        agents.iter().all(|a| a["name"] != "bad"),
        "rejected create must not leave rows behind"
    );
}
