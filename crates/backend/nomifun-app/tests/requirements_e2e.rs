//! E2E tests for the Requirements Platform HTTP endpoints.

mod common;

use axum::http::StatusCode;
use nomifun_common::ConversationId;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

const MISSING_REQUIREMENT_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679994";
const MISSING_TERMINAL_ID: &str = "0190f5fe-7c00-7a00-8abc-012345679995";

#[tokio::test]
async fn unauthenticated_list_is_rejected() {
    let (app, _services) = build_app().await;
    let resp = app.oneshot(get_request("/api/requirements")).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "expected 401/403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn create_list_get_update_delete_happy_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // create
    let body = json!({ "title": "E2E", "content": "x", "tag": "e2e", "order_key": "1" });
    let resp = app
        .clone()
        .oneshot(json_with_token("POST", "/api/requirements", body, &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["status"], "pending");
    assert_eq!(json["data"]["display_no"], 1);
    let requirement_id = json["data"]["requirement_id"]
        .as_str()
        .expect("requirement exposes a stable business UUID")
        .to_owned();
    assert!(nomifun_common::validate_uuidv7(&requirement_id).is_ok());

    // list (filtered by tag)
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/requirements?tag=e2e", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["total"], 1);
    assert_eq!(json["data"]["items"][0]["requirement_id"], requirement_id);
    assert_eq!(json["data"]["items"][0]["display_no"], 1);

    // get
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/requirements/{requirement_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await["data"]["requirement_id"],
        requirement_id
    );

    // update → done
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/requirements/{requirement_id}"),
            json!({ "status": "done", "completion_note": "ok" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["status"], "done");

    // board
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/requirements/board?tag=e2e", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["done"].as_array().unwrap().len(), 1);

    // tags
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/requirements/tags", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let e2e = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tag"] == "e2e")
        .expect("e2e tag summary present");
    assert_eq!(e2e["done"], 1);

    // delete
    let resp = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/requirements/{requirement_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // get → 404
    let resp = app
        .oneshot(get_with_token(
            &format!("/api/requirements/{requirement_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_missing_title_is_400() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/requirements",
            json!({ "title": "", "tag": "e2e" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn get_unknown_is_404() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let resp = app
        .oneshot(get_with_token(
            &format!("/api/requirements/{MISSING_REQUIREMENT_ID}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
}

/// Seed a conversation row so the logical conversation reference set by claim
/// resolves to an existing conversation.
async fn seed_conversation(services: &nomifun_app::AppServices, conv_id: &str) {
    sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, extra, created_at, updated_at) \
         VALUES (?, ?, 'Dispatch Conv', 'nomi', '{}', 0, 0)",
    )
    .bind(conv_id)
    .bind(services.authoritative_user_id.as_ref())
    .execute(services.database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn claim_complete_and_drain() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = ConversationId::new().into_string();
    seed_conversation(&services, &conv).await;

    // Seed two requirements in tag "disp".
    for (title, order) in [("A", "1"), ("B", "2")] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/requirements",
                json!({ "title": title, "tag": "disp", "order_key": order }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Claim → lowest order (A) goes in_progress.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/claim",
            json!({ "tag": "disp", "conversation_id": conv }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["title"], "A");
    assert_eq!(json["data"]["status"], "in_progress");
    let a_id = json["data"]["requirement_id"]
        .as_str()
        .expect("requirement exposes a stable business UUID");

    // Complete A.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/requirements/{a_id}/complete"),
            json!({ "completion_note": "ok" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["status"], "done");

    // Claim again → B.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/claim",
            json!({ "tag": "disp", "conversation_id": conv }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["title"], "B");

    // Claim again → drained (data == null).
    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/claim",
            json!({ "tag": "disp", "conversation_id": conv }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["data"].is_null(), "tag drained → null");
}

#[tokio::test]
async fn set_autowork_requires_tag_when_enabled() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let conv = ConversationId::new().into_string();
    seed_conversation(&services, &conv).await;

    // enabled without tag → 400.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "target_id": conv, "enabled": true }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // disabled → 200, not running, run_state off.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "target_id": conv, "enabled": false }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["enabled"], false);
    assert_eq!(json["data"]["running"], false);
    assert_eq!(json["data"]["run_state"], "off");
    assert_eq!(json["data"]["kind"], "conversation");

    // GET reflects disabled (kind/target_id path form).
    let resp = app
        .oneshot(get_with_token(
            &format!("/api/requirements/autowork/conversation/{conv}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["enabled"], false);
}

#[tokio::test]
async fn terminal_autowork_unknown_terminal_is_not_found() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Enabling AutoWork on a non-existent terminal → ownership check 404.
    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "kind": "terminal", "target_id": MISSING_TERMINAL_ID, "enabled": true, "tag": "x" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn autowork_unknown_kind_is_bad_request() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let resp = app
        .oneshot(get_with_token("/api/requirements/autowork/bogus/term_1", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn terminal_autowork_rejects_plain_shell() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create a plain-shell terminal (no agent backend).
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/terminals",
            json!({ "cwd": std::env::temp_dir().to_string_lossy(), "command": "$SHELL", "cols": 80, "rows": 24 }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let term_id = body_json(resp).await["data"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Enabling AutoWork on a plain shell → eligibility check 400.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/autowork",
            json!({ "kind": "terminal", "target_id": term_id, "enabled": true, "tag": "x" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Cleanup: kill/remove the spawned shell.
    let _ = app
        .oneshot(json_with_token(
            "DELETE",
            &format!("/api/terminals/{term_id}"),
            json!({}),
            &token,
            &csrf,
        ))
        .await;
}

#[tokio::test]
async fn batch_delete_removes_selected() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create three requirements; collect their ids.
    let mut ids = Vec::new();
    for (title, order) in [("A", "1"), ("B", "2"), ("C", "3")] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/requirements",
                json!({ "title": title, "tag": "batch", "order_key": order }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = body_json(resp).await["data"]["requirement_id"]
            .as_str()
            .expect("requirement exposes a stable business UUID")
            .to_owned();
        ids.push(id);
    }

    // Batch-delete the first two (plus a non-existent business UUID, which is skipped).
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/batch-delete",
            json!({ "ids": [ids[0], ids[1], MISSING_REQUIREMENT_ID] }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["deleted"], 2);

    // Only "C" remains.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/requirements?tag=batch", &token))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["total"], 1);
    assert_eq!(json["data"]["items"][0]["title"], "C");

    // Empty ids → 400.
    let resp = app
        .oneshot(json_with_token(
            "POST",
            "/api/requirements/batch-delete",
            json!({ "ids": [] }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
