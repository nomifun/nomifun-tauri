//! E2E tests for the multi-companion REST surface: `/api/companion/companions*` CRUD and the
//! per-companion companion thread routes (T2.1).
//!
//! The companion roster persists on disk under the shared test data dir, so the
//! assertions are id-scoped (find-by-id / 404-after-delete) and never assume
//! an empty roster or absolute counts.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, delete_with_token, get_with_token, json_with_token, setup_and_login};

const MISSING_COMPANION_ID: &str =
    "0190f5fe-7c00-7a00-8000-000000000099";

async fn seed_test_provider(services: &nomifun_app::AppServices) {
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES ('0190f5fe-7c00-7a00-8000-000000000012', 'openai', 'test', 'https://example.invalid', \
                   'encrypted', '[\"test-model\"]', 1, '[]', 1, 1)",
    )
    .execute(services.database.pool())
    .await
    .unwrap();
}

/// POST /api/companion/companions and return the created profile JSON (asserts 201).
async fn create_companion(
    app: &axum::Router,
    token: &str,
    csrf: &str,
    name: &str,
    character: &str,
) -> serde_json::Value {
    let req = json_with_token(
        "POST",
        "/api/companion/companions",
        json!({ "name": name, "character": character }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    json["data"].clone()
}

// ── companions CRUD ─────────────────────────────────────────────────────────

#[tokio::test]
async fn companions_crud_happy_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create.
    let created = create_companion(&app, &token, &csrf, "毛球", "ink").await;
    let id = created["companion_id"].as_str().unwrap().to_owned();
    nomifun_common::CompanionId::parse(&id).unwrap();
    assert_eq!(created["name"], "毛球");
    assert_eq!(created["character"], "ink");

    // List: profile fields flattened + embedded status.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/companion/companions", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let entry = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["companion_id"] == id.as_str())
        .expect("created companion should appear in the list")
        .clone();
    assert_eq!(entry["name"], "毛球");
    assert_eq!(entry["status"]["companion_id"], id.as_str());
    assert!(entry["status"]["level"].as_i64().unwrap() >= 1);

    // Detail: same flattened shape.
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/companion/companions/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = body_json(resp).await;
    assert_eq!(detail["data"]["companion_id"], id.as_str());
    assert_eq!(detail["data"]["character"], "ink");
    assert_eq!(detail["data"]["status"]["companion_id"], id.as_str());

    // RFC 7396 patch: rename + nested appearance merge.
    let req = json_with_token(
        "PATCH",
        &format!("/api/companion/companions/{id}"),
        json!({ "name": "新名", "appearance": { "companion_enabled": true } }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let patched = body_json(resp).await;
    assert_eq!(patched["data"]["companion_id"], id.as_str());
    assert_eq!(patched["data"]["name"], "新名");
    assert_eq!(patched["data"]["appearance"]["companion_enabled"], true);
    // Untouched field survives the merge.
    assert_eq!(patched["data"]["character"], "ink");

    // Per-companion status endpoint.
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/companion/companions/{id}/status"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = body_json(resp).await;
    assert_eq!(status["data"]["companion_id"], id.as_str());

    // Delete: 204 and the companion is gone.
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/companion/companions/{id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/companion/companions/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn companions_unknown_id_is_404_and_bad_name_is_400() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // 404 on every per-companion verb for an unknown id.
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/companion/companions/{MISSING_COMPANION_ID}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/companion/companions/{MISSING_COMPANION_ID}/status"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let req = json_with_token(
        "PATCH",
        &format!("/api/companion/companions/{MISSING_COMPANION_ID}"),
        json!({ "name": "x" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/companion/companions/{MISSING_COMPANION_ID}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 400 on invalid names (service-level validation).
    let req = json_with_token("POST", "/api/companion/companions", json!({ "name": "   " }), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let req = json_with_token(
        "POST",
        "/api/companion/companions",
        json!({ "name": "x".repeat(41) }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── per-companion companion threads ─────────────────────────────────────────

#[tokio::test]
async fn companion_single_session_happy_path() {
    let (mut app, services) = build_app().await;
    seed_test_provider(&services).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let companion = create_companion(&app, &token, &csrf, "甲", "ink").await;
    let id = companion["companion_id"].as_str().unwrap().to_owned();

    // Without a configured model the companion cannot open its session (400).
    let req = json_with_token(
        "POST",
        &format!("/api/companion/companions/{id}/companion/threads"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Configure the model. The work-partner is a SINGLE-session model now:
    // configuring the model auto-ensures the one companion session.
    let req = json_with_token(
        "PATCH",
        &format!("/api/companion/companions/{id}"),
        json!({ "model": { "provider_id": "0190f5fe-7c00-7a00-8000-000000000012", "model": "test-model" } }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // POST create-thread is an idempotent ensure of that single session; it
    // returns the conversation bound to this companion.
    let req = json_with_token(
        "POST",
        &format!("/api/companion/companions/{id}/companion/threads"),
        json!({ "title": "第一聊" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let thread = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "ensure companion session failed: {thread}");
    let conv = thread["data"]["conversation_id"].as_str().unwrap().to_owned();
    assert!(!conv.is_empty());
    assert_eq!(thread["data"]["companion_id"], id.as_str());

    // GET active points at that same single session.
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/companion/companions/{id}/companion/active"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let active = body_json(resp).await;
    assert_eq!(active["data"]["conversation_id"], conv.as_str());

    // Re-ensuring is idempotent — the same conversation, never a second one.
    let req = json_with_token(
        "POST",
        &format!("/api/companion/companions/{id}/companion/threads"),
        json!({ "title": "忽略" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let again = body_json(resp).await;
    assert_eq!(again["data"]["conversation_id"], conv.as_str());
}

#[tokio::test]
async fn companion_thread_unknown_companion_404_and_no_model_400() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Unknown companion: the single-session surface 404s (existence-gated)
    // instead of reading as "no active session".
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!(
                "/api/companion/companions/{MISSING_COMPANION_ID}/companion/active"
            ),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let req = json_with_token(
        "POST",
        &format!(
            "/api/companion/companions/{MISSING_COMPANION_ID}/companion/threads"
        ),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // A known companion without a configured model cannot open its session (400).
    let b = create_companion(&app, &token, &csrf, "乙", "boo").await;
    let b_id = b["companion_id"].as_str().unwrap().to_owned();
    let req = json_with_token(
        "POST",
        &format!("/api/companion/companions/{b_id}/companion/threads"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
