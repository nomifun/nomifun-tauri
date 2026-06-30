//! E2E tests for the 智能编排 (orchestration) fleet + workspace CRUD endpoints.
//!
//! Proves the orchestrator routes are mounted under the app's auth middleware
//! and that the full HTTP CRUD round-trips against a real in-memory database.
//! Mirrors `webhook_e2e.rs` for the test-app + authenticated-request pattern.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn unauthenticated_fleet_list_is_rejected() {
    let (app, _services) = build_app().await;
    let resp = app.oneshot(get_request("/api/orchestrator/fleets")).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401/403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn fleet_crud_round_trips() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // create (with one member)
    let body = json!({
        "name": "研究编队",
        "description": "multi-agent",
        "max_parallel": 3,
        "members": [
            { "agent_id": "agent_builtin_claude", "role_hint": "后端" }
        ]
    });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/orchestrator/fleets", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["id"].as_str().expect("fleet id is a string").to_owned();
    assert!(id.starts_with("fleet_"), "id should be a fleet_ string, got {id}");
    assert_eq!(json["data"]["name"], "研究编队");
    assert_eq!(json["data"]["max_parallel"], 3);
    assert_eq!(json["data"]["members"].as_array().unwrap().len(), 1);
    assert_eq!(json["data"]["members"][0]["agent_id"], "agent_builtin_claude");
    assert_eq!(json["data"]["members"][0]["sort_order"], 0);

    // list contains it
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/orchestrator/fleets", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let fleets = json["data"].as_array().unwrap();
    assert_eq!(fleets.len(), 1);
    assert_eq!(fleets[0]["id"], id);

    // get by id
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/orchestrator/fleets/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["name"], "研究编队");

    // update: rename
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/orchestrator/fleets/{id}"),
            json!({ "name": "改名编队" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "改名编队");
    // member set untouched when `members` absent from the patch.
    assert_eq!(json["data"]["members"].as_array().unwrap().len(), 1);

    // delete
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/orchestrator/fleets/{id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // get after delete → 404
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/orchestrator/fleets/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fleet_create_validates_members_required() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    // a fleet with no members → 400 (service rejects an empty member set).
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/orchestrator/fleets",
            json!({ "name": "空编队", "members": [] }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn workspace_crud_round_trips() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // create (no default fleet — keeps the FK NULL so no fleet is needed)
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/orchestrator/workspaces",
            json!({ "name": "研究工作区", "workspace_dir": "/tmp/ws" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    let id = json["data"]["id"].as_str().expect("workspace id is a string").to_owned();
    assert!(id.starts_with("ows_"), "id should be an ows_ string, got {id}");
    assert_eq!(json["data"]["name"], "研究工作区");
    assert_eq!(json["data"]["workspace_dir"], "/tmp/ws");

    // list contains it
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/orchestrator/workspaces", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"].as_array().unwrap().len(), 1);

    // update: rename
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/orchestrator/workspaces/{id}"),
            json!({ "name": "改名工作区" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["name"], "改名工作区");

    // delete
    let resp = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/orchestrator/workspaces/{id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // get after delete → 404
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/orchestrator/workspaces/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
